//! The property cells and their bindings (`docs/UI-FRAMEWORK.md` §4.1).
//!
//! A property is a cell in [`Ui`]'s arena: its value, an optional binding,
//! a dirty flag, and the cells it read and that read it. A binding is a
//! closure given the whole [`Ui`]; every property it reads while it runs
//! is recorded as an edge, so the next change to any of them marks it
//! dirty. Marking does no work but the marking: a dirty binding runs the
//! next time its property is read, so a burst of changes costs one
//! evaluation. Setting a property by hand removes its binding, as
//! assigning to a bound property in QML does.
//!
//! One thread, as Qt's GUI thread: nothing here is `Send` and no lock is
//! taken.

use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::item::{ItemId, Slot};
use crate::text::TextEngine;
use crate::value::{PropType, Value};

/// A property cell's id: an index into the arena and the generation it
/// was made in, so the id of a destroyed property never reaches the cell
/// that took its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PropId {
    /// The index.
    index: u32,
    /// Its generation.
    generation: u32,
}

/// A typed property: a [`PropId`] that reads and writes as `T`.
#[derive(Debug)]
pub struct Prop<T> {
    /// The cell.
    pub id: PropId,
    /// Its type.
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for Prop<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Prop<T> {}

impl<T> PartialEq for Prop<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<T> Eq for Prop<T> {}

impl<T: PropType> Prop<T> {
    /// Its value, recorded as read by the binding running now.
    pub fn get(self, ui: &mut Ui) -> T {
        ui.get(self)
    }

    /// Set it, dropping its binding.
    pub fn set(self, ui: &mut Ui, value: T) {
        ui.set(self, value);
    }

    /// Bind it to `binding`.
    pub fn bind(self, ui: &mut Ui, binding: impl Fn(&mut Ui) -> T + 'static) {
        ui.bind(self, binding);
    }
}

/// A binding: the closure a property's value is evaluated by.
pub(crate) type Binding = Rc<dyn Fn(&mut Ui) -> Value>;

/// One property cell.
pub(crate) struct Cell {
    /// Bumped each time the cell is freed.
    generation: u32,
    /// Whether a property holds it.
    live: bool,
    /// The value.
    value: Value,
    /// What the value is evaluated by, if anything.
    binding: Option<Binding>,
    /// Whether the binding must run again before the value is read.
    dirty: bool,
    /// Whether the binding is running now: reached again, it is a loop.
    busy: bool,
    /// The bindings that read it.
    readers: Vec<PropId>,
    /// What its binding read the last time it ran.
    deps: Vec<PropId>,
}

impl fmt::Debug for Cell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cell")
            .field("live", &self.live)
            .field("value", &self.value)
            .field("bound", &self.binding.is_some())
            .field("dirty", &self.dirty)
            .finish_non_exhaustive()
    }
}

/// A user interface: its properties, its items, its fonts.
pub struct Ui {
    /// The property cells.
    cells: Vec<Cell>,
    /// Cells free to be taken again.
    free_cells: Vec<u32>,
    /// The items (`crate::item`).
    pub(crate) items: Vec<Slot>,
    /// Item slots free to be taken again.
    pub(crate) free_items: Vec<u32>,
    /// The items with no parent, in the order they were made: each is a
    /// window's content, painted in this order.
    pub(crate) roots: Vec<ItemId>,
    /// The bindings running now, innermost last.
    running: Vec<PropId>,
    /// The properties whose binding was found reading itself.
    loops: Vec<PropId>,
    /// Fonts and shaped text.
    pub(crate) text: TextEngine,
    /// Buffer pixels to a logical pixel.
    pub(crate) scale: Prop<f32>,
    /// Whether anything changed since the last frame.
    changed: bool,
}

impl fmt::Debug for Ui {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ui")
            .field("cells", &self.cells.len())
            .field("items", &self.items.len())
            .field("roots", &self.roots)
            .field("loops", &self.loops)
            .finish_non_exhaustive()
    }
}

impl Ui {
    /// A user interface drawing text with `fonts`.
    #[must_use]
    pub fn new(fonts: compositor_text::Fonts) -> Self {
        let mut ui = Ui {
            cells: Vec::new(),
            free_cells: Vec::new(),
            items: Vec::new(),
            free_items: Vec::new(),
            roots: Vec::new(),
            running: Vec::new(),
            loops: Vec::new(),
            text: TextEngine::new(fonts),
            scale: Prop {
                id: PropId {
                    index: 0,
                    generation: 0,
                },
                marker: PhantomData,
            },
            changed: true,
        };
        ui.scale = ui.prop(1.0_f32);
        ui
    }

    /// A new property holding `value`.
    pub fn prop<T: PropType>(&mut self, value: T) -> Prop<T> {
        Prop {
            id: self.alloc(value.into_value()),
            marker: PhantomData,
        }
    }

    /// The scale frames are drawn at: buffer pixels to a logical one.
    pub fn scale(&mut self) -> f32 {
        self.get(self.scale)
    }

    /// Draw at `scale` from now on.
    pub fn set_scale(&mut self, scale: f32) {
        self.set(self.scale, scale);
    }

    /// `prop`'s value, read by the binding running now if there is one.
    pub fn get<T: PropType>(&mut self, prop: Prop<T>) -> T {
        T::from_value(&self.read(prop.id)).unwrap_or_else(T::fallback)
    }

    /// Set `prop` to `value`, dropping its binding.
    pub fn set<T: PropType>(&mut self, prop: Prop<T>, value: T) {
        self.write(prop.id, value.into_value());
    }

    /// Bind `prop` to `binding`, which runs the next time it is read.
    pub fn bind<T: PropType>(&mut self, prop: Prop<T>, binding: impl Fn(&mut Ui) -> T + 'static) {
        let binding: Binding = Rc::new(move |ui: &mut Ui| binding(ui).into_value());
        self.attach(prop.id, binding);
    }

    /// Whether anything changed since [`Ui::take_changed`] last asked.
    pub fn take_changed(&mut self) -> bool {
        core::mem::replace(&mut self.changed, false)
    }

    /// The properties whose binding read itself, directly or through
    /// others; each kept its last value.
    #[must_use]
    pub fn binding_loops(&self) -> &[PropId] {
        &self.loops
    }

    /// Whether `id` names a live cell.
    #[must_use]
    pub fn is_live(&self, id: PropId) -> bool {
        self.cell(id).is_some()
    }

    /// The live cell `id` names.
    fn cell(&self, id: PropId) -> Option<&Cell> {
        self.cells
            .get(id.index as usize)
            .filter(|cell| cell.live && cell.generation == id.generation)
    }

    /// The same, to change.
    fn cell_mut(&mut self, id: PropId) -> Option<&mut Cell> {
        self.cells
            .get_mut(id.index as usize)
            .filter(|cell| cell.live && cell.generation == id.generation)
    }

    /// A cell holding `value`.
    pub(crate) fn alloc(&mut self, value: Value) -> PropId {
        if let Some(index) = self.free_cells.pop()
            && let Some(cell) = self.cells.get_mut(index as usize)
        {
            cell.live = true;
            cell.value = value;
            cell.dirty = false;
            cell.busy = false;
            return PropId {
                index,
                generation: cell.generation,
            };
        }
        let index = u32::try_from(self.cells.len()).unwrap_or(u32::MAX);
        self.cells.push(Cell {
            generation: 0,
            live: true,
            value,
            binding: None,
            dirty: false,
            busy: false,
            readers: Vec::new(),
            deps: Vec::new(),
        });
        PropId {
            index,
            generation: 0,
        }
    }

    /// Free `id`: whatever read it is dirty and will read nothing there.
    pub(crate) fn free(&mut self, id: PropId) {
        self.unlink(id);
        self.mark_readers(id);
        let readers = match self.cell_mut(id) {
            Some(cell) => {
                cell.live = false;
                cell.generation = cell.generation.wrapping_add(1);
                cell.value = Value::Void;
                cell.binding = None;
                core::mem::take(&mut cell.readers)
            }
            None => return,
        };
        for reader in readers {
            if let Some(cell) = self.cell_mut(reader) {
                cell.deps.retain(|&dep| dep != id);
            }
        }
        self.free_cells.push(id.index);
    }

    /// `id`'s value, settled, and recorded as read by the running binding.
    pub(crate) fn read(&mut self, id: PropId) -> Value {
        if let Some(&reader) = self.running.last()
            && reader != id
        {
            self.link(reader, id);
        }
        self.settle(id);
        self.cell(id)
            .map(|cell| cell.value.clone())
            .unwrap_or_default()
    }

    /// Run `id`'s binding if it is dirty.
    fn settle(&mut self, id: PropId) {
        let binding = match self.cell_mut(id) {
            Some(cell) if cell.dirty && cell.busy => {
                // Reached again while it runs: a loop. It keeps its last
                // value, and the loop is said once.
                cell.dirty = false;
                if !self.loops.contains(&id) {
                    self.loops.push(id);
                }
                return;
            }
            Some(cell) if cell.dirty => match &cell.binding {
                Some(binding) => {
                    cell.busy = true;
                    Rc::clone(binding)
                }
                None => {
                    cell.dirty = false;
                    return;
                }
            },
            _ => return,
        };
        self.unlink(id);
        self.running.push(id);
        let value = binding(self);
        let _ = self.running.pop();
        if let Some(cell) = self.cell_mut(id) {
            cell.busy = false;
            cell.dirty = false;
            cell.value = value;
        }
    }

    /// Set `id` by hand.
    pub(crate) fn write(&mut self, id: PropId, value: Value) {
        self.unlink(id);
        let Some(cell) = self.cell_mut(id) else {
            return;
        };
        cell.binding = None;
        cell.dirty = false;
        if cell.value == value {
            return;
        }
        cell.value = value;
        self.mark_readers(id);
    }

    /// Give `id` a binding.
    pub(crate) fn attach(&mut self, id: PropId, binding: Binding) {
        self.unlink(id);
        let Some(cell) = self.cell_mut(id) else {
            return;
        };
        cell.binding = Some(binding);
        cell.dirty = true;
        self.mark_readers(id);
    }

    /// Record that `reader`'s binding read `dep`.
    fn link(&mut self, reader: PropId, dep: PropId) {
        let Some(cell) = self.cell_mut(dep) else {
            return;
        };
        if !cell.readers.contains(&reader) {
            cell.readers.push(reader);
        }
        if let Some(cell) = self.cell_mut(reader)
            && !cell.deps.contains(&dep)
        {
            cell.deps.push(dep);
        }
    }

    /// Forget what `id`'s binding read.
    fn unlink(&mut self, id: PropId) {
        let deps = match self.cell_mut(id) {
            Some(cell) => core::mem::take(&mut cell.deps),
            None => return,
        };
        for dep in deps {
            if let Some(cell) = self.cell_mut(dep) {
                cell.readers.retain(|&reader| reader != id);
            }
        }
    }

    /// Mark every binding that read `id`, and theirs, dirty.
    fn mark_readers(&mut self, id: PropId) {
        self.changed = true;
        let mut stack: Vec<PropId> = self
            .cell(id)
            .map(|cell| cell.readers.clone())
            .unwrap_or_default();
        while let Some(reader) = stack.pop() {
            if let Some(cell) = self.cell_mut(reader)
                && !cell.dirty
            {
                cell.dirty = true;
                stack.extend(cell.readers.iter().copied());
            }
        }
    }
}
