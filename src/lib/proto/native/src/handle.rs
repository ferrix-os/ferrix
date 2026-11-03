//! Owning a handle, and giving it up.
//!
//! A handle a program holds is closed when its owner is dropped, the way a
//! `File` closes its descriptor. Every object type in this crate is an
//! [`OwnedHandle`] underneath, and [`Object`] is what they share: the handle,
//! the way to make calls, and the two waits every object answers.
//!
//! # Why adopting a raw handle is safe here
//!
//! `std`'s `OwnedFd::from_raw_fd` is `unsafe`, because a descriptor number is
//! reused as soon as it is closed: adopting one you do not own can close a file
//! somebody else opens later. A handle is different. `src/lib/kernel/objects` gives a
//! slot a new generation every time it is reused and retires it rather than
//! wrap, so a value once closed never names anything again; adopting a handle
//! you do not own can close a handle somebody else holds — a logic error, the
//! kernel answers the second close with `BAD_HANDLE` — and cannot reach memory
//! at all. Handles are not memory, so [`OwnedHandle::from_raw`] is safe.

use core::mem::ManuallyDrop;

use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Requested, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;

use crate::call::{Call, Syscall};
use crate::error::{Error, decode, decode_handle, decode_unit};

/// When a wait gives up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    /// Never: wait until the signal comes.
    Never,
    /// At this absolute `CLOCK_MONOTONIC` time, in nanoseconds.
    At(u64),
}

impl Deadline {
    /// The bytes a deadline pointer names, or `None` for a null pointer.
    pub(crate) const fn bytes(self) -> Option<[u8; 8]> {
        match self {
            Deadline::Never => None,
            Deadline::At(nanos) => Some(nanos.to_ne_bytes()),
        }
    }
}

/// The register value a handle travels in.
pub(crate) const fn register(handle: Handle) -> usize {
    handle.0 as usize
}

/// The register value that asks for `requested`.
#[must_use]
pub const fn rights_register(requested: Requested) -> usize {
    match requested {
        Requested::Same => SAME_RIGHTS as usize,
        Requested::Exactly(rights) => rights.0 as usize,
    }
}

/// A handle this program holds, closed when it is dropped.
#[derive(Debug)]
pub struct OwnedHandle<S: Syscall> {
    /// The value. [`Handle::INVALID`] is never closed.
    handle: Handle,
    /// How to close it.
    sys: S,
}

impl<S: Syscall> OwnedHandle<S> {
    /// Take ownership of `handle`: one received in a message, or the bootstrap
    /// handle the runtime was started with.
    ///
    /// Safe, for the reason the module documentation gives.
    #[must_use]
    pub const fn from_raw(sys: S, handle: Handle) -> OwnedHandle<S> {
        OwnedHandle { handle, sys }
    }

    /// The handle's value, still owned here.
    #[must_use]
    pub const fn raw(&self) -> Handle {
        self.handle
    }

    /// The way this handle makes calls.
    #[must_use]
    pub const fn syscall(&self) -> S {
        self.sys
    }

    /// Stop owning the handle without closing it, and return its value: for
    /// a handle the kernel has taken, such as one sent through a channel.
    #[must_use]
    pub fn into_raw(self) -> Handle {
        ManuallyDrop::new(self).handle
    }

    /// Close the handle and say whether the kernel agreed.
    ///
    /// Dropping closes it too, and ignores the answer.
    ///
    /// # Errors
    ///
    /// [`Error::BadHandle`] if it was not open.
    pub fn close(self) -> Result<(), Error> {
        let sys = self.sys;
        close(sys, self.into_raw())
    }

    /// A second handle to the same object, with the same or fewer rights.
    ///
    /// # Errors
    ///
    /// [`Error::AccessDenied`] without `DUPLICATE` or for a right not held;
    /// [`Error::InvalidArgs`] for an undefined right; [`Error::NoHandles`].
    pub fn duplicate(&self, rights: Requested) -> Result<OwnedHandle<S>, Error> {
        let value = Call::new(nr::HANDLE_DUPLICATE)
            .value(register(self.handle))
            .value(rights_register(rights))
            .make(self.sys);
        decode_handle(value).map(|handle| OwnedHandle::from_raw(self.sys, handle))
    }

    /// Swap this handle for one with the same or fewer rights.
    ///
    /// # Errors
    ///
    /// As [`OwnedHandle::duplicate`], less the need for `DUPLICATE`. A failed
    /// replace changes nothing in the kernel, so the original comes back.
    pub fn replace(self, rights: Requested) -> Result<OwnedHandle<S>, (Error, OwnedHandle<S>)> {
        let value = Call::new(nr::HANDLE_REPLACE)
            .value(register(self.handle))
            .value(rights_register(rights))
            .make(self.sys);
        match decode(value) {
            Err(error) => Err((error, self)),
            Ok(_) => {
                let sys = self.sys;
                // The original is closed by the kernel either way now, so it
                // must not be closed again here.
                let _ = self.into_raw();
                decode_handle(value)
                    .map(|handle| OwnedHandle::from_raw(sys, handle))
                    .map_err(|error| (error, OwnedHandle::from_raw(sys, Handle::INVALID)))
            }
        }
    }
}

impl<S: Syscall> Drop for OwnedHandle<S> {
    fn drop(&mut self) {
        if self.handle.is_valid() {
            let _ = close(self.sys, self.handle);
        }
    }
}

/// `handle_close`.
fn close<S: Syscall>(sys: S, handle: Handle) -> Result<(), Error> {
    decode_unit(
        Call::new(nr::HANDLE_CLOSE)
            .value(register(handle))
            .make(sys),
    )
}

/// What every kind of handle shares.
pub trait Object<S: Syscall> {
    /// The handle's value.
    fn handle(&self) -> Handle;

    /// The way this handle makes calls.
    fn syscall(&self) -> S;

    /// `object_wait_one`: block until any of `signals` is asserted or the
    /// deadline passes, and return the signals asserted then.
    ///
    /// # Errors
    ///
    /// [`Error::TimedOut`]; [`Error::AccessDenied`] without `WAIT`;
    /// [`Error::InvalidArgs`] for an undefined signal; [`Error::Interrupted`]
    /// if the process is killed while it waits.
    fn wait_one(&self, signals: Signals, deadline: Deadline) -> Result<Signals, Error> {
        let deadline = deadline.bytes();
        let mut observed = [0_u8; 4];
        let value = Call::new(nr::OBJECT_WAIT_ONE)
            .value(register(self.handle()))
            .value(signals.0 as usize)
            .optional_input(deadline.as_ref().map(<[u8; 8]>::as_slice))
            .output(&mut observed)
            .make(self.syscall());
        decode_unit(value).map(|()| Signals(u32::from_ne_bytes(observed)))
    }

    /// `object_wait_async`: queue one packet carrying `key` on `port` the next
    /// time any of `signals` is asserted.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for an object whose signals never change;
    /// [`Error::AccessDenied`] without `WAIT` here or `WRITE` on the port;
    /// [`Error::InvalidArgs`] for no signals, an undefined one, or `WRITABLE`.
    fn wait_async(
        &self,
        port: &crate::port::Port<S>,
        signals: Signals,
        key: u64,
    ) -> Result<(), Error> {
        let key = key.to_ne_bytes();
        let value = Call::new(nr::OBJECT_WAIT_ASYNC)
            .value(register(self.handle()))
            .value(register(port.handle()))
            .value(signals.0 as usize)
            .input(&key)
            .make(self.syscall());
        decode_unit(value)
    }

    /// `object_id`: the id of the process or job this handle names, which a
    /// sender's identity from `channel_read_attested` is compared with
    /// (`docs/microkernel/K6.md`). A process's is never given to another
    /// process, and is answered after it has ended too.
    ///
    /// # Errors
    ///
    /// [`Error::WrongType`] for any other object; [`Error::BadHandle`].
    fn object_id(&self) -> Result<u64, Error> {
        let mut id = [0_u8; 8];
        let value = Call::new(nr::OBJECT_ID)
            .value(register(self.handle()))
            .output(&mut id)
            .make(self.syscall());
        decode_unit(value).map(|()| u64::from_ne_bytes(id))
    }
}

impl<S: Syscall> Object<S> for OwnedHandle<S> {
    fn handle(&self) -> Handle {
        self.handle
    }

    fn syscall(&self) -> S {
        self.sys
    }
}

/// Declare a kind of object: an [`OwnedHandle`] that knows what it names.
macro_rules! object_handle {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug)]
        pub struct $name<S: $crate::call::Syscall>($crate::handle::OwnedHandle<S>);

        impl<S: $crate::call::Syscall> $name<S> {
            /// Treat an owned handle as naming this kind of object.
            ///
            /// Nothing is checked here: a handle to something else makes each
            /// call fail with `Error::WrongType`, which is how the kernel says so.
            #[must_use]
            pub const fn from_owned(handle: $crate::handle::OwnedHandle<S>) -> Self {
                Self(handle)
            }

            /// The owned handle underneath.
            #[must_use]
            pub fn into_owned(self) -> $crate::handle::OwnedHandle<S> {
                self.0
            }

            /// The owned handle underneath, borrowed.
            #[must_use]
            pub const fn as_owned(&self) -> &$crate::handle::OwnedHandle<S> {
                &self.0
            }

            /// Close the handle and say whether the kernel agreed.
            ///
            /// # Errors
            ///
            /// As `OwnedHandle::close`.
            pub fn close(self) -> Result<(), $crate::error::Error> {
                self.0.close()
            }

            /// A second handle to the same object, with the same or fewer rights.
            ///
            /// # Errors
            ///
            /// As `OwnedHandle::duplicate`.
            pub fn duplicate(
                &self,
                rights: ferrix_native_abi::rights::Requested,
            ) -> Result<Self, $crate::error::Error> {
                self.0.duplicate(rights).map(Self)
            }

            /// Swap the handle for one with the same or fewer rights.
            ///
            /// # Errors
            ///
            /// As `OwnedHandle::replace`, which gives the original back.
            pub fn replace(
                self,
                rights: ferrix_native_abi::rights::Requested,
            ) -> Result<Self, ($crate::error::Error, Self)> {
                self.0
                    .replace(rights)
                    .map(Self)
                    .map_err(|(error, handle)| (error, Self(handle)))
            }
        }

        impl<S: $crate::call::Syscall> $crate::handle::Object<S> for $name<S> {
            fn handle(&self) -> ferrix_native_abi::handle::Handle {
                self.0.raw()
            }

            fn syscall(&self) -> S {
                self.0.syscall()
            }
        }
    };
}

pub(crate) use object_handle;
