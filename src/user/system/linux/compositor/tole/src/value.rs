//! What a property holds: the markup's types (`docs/UI-FRAMEWORK.md`
//! §3.1) as one enum the property cells store, and the Rust types each one
//! is read and written as.

use std::rc::Rc;

/// A colour, not premultiplied, as the markup writes it (`#rrggbb`,
/// `#rrggbbaa`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha, 255 opaque.
    pub a: u8,
}

impl Color {
    /// Fully transparent.
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);

    /// A colour from its four channels.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color { r, g, b, a }
    }

    /// An opaque colour from `0xRRGGBB`.
    #[must_use]
    pub const fn hex(rgb: u32) -> Self {
        Color::rgba((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, 255)
    }

    /// The same colour at `alpha`, 0 to 1.
    #[must_use]
    pub fn alpha(self, alpha: f32) -> Self {
        Color {
            a: (alpha.clamp(0.0, 1.0) * 255.0) as u8,
            ..self
        }
    }

    /// Whether nothing of it shows.
    #[must_use]
    pub const fn is_transparent(self) -> bool {
        self.a == 0
    }
}

/// A font as a property holds it: the families, best first and comma
/// separated as Pango writes them, a size in logical pixels and a weight.
#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    /// `"IBM Plex Sans, Inter, sans-serif"`.
    pub families: Rc<str>,
    /// Logical pixels.
    pub px: f32,
    /// 100 to 900; 400 regular.
    pub weight: u16,
}

impl Font {
    /// `families` at `px` and `weight`.
    #[must_use]
    pub fn new(families: &str, px: f32, weight: u16) -> Self {
        Font {
            families: Rc::from(families),
            px,
            weight,
        }
    }
}

impl Default for Font {
    fn default() -> Self {
        Font::new("sans-serif", 14.0, 400)
    }
}

/// Where a line of text is cut when it does not fit, as `Text.elide`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Elide {
    /// Not cut: it runs past the item's edge.
    #[default]
    None,
    /// Cut at the end, with `…`.
    Right,
}

/// Where text sits across its item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HAlign {
    /// At the left edge.
    #[default]
    Left,
    /// Centred.
    Center,
    /// At the right edge.
    Right,
}

/// Where text sits down its item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VAlign {
    /// At the top.
    Top,
    /// Centred: the line's ascent and descent in the middle, as the lode
    /// viewer places its labels.
    #[default]
    Center,
    /// At the bottom.
    Bottom,
}

/// A property's value.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Value {
    /// Nothing yet: a property whose binding has not run, or one read
    /// after its item was destroyed.
    #[default]
    Void,
    /// `bool`.
    Bool(bool),
    /// `int`.
    Int(i32),
    /// `real` and `length`.
    Real(f32),
    /// `string`.
    Str(Rc<str>),
    /// `color`.
    Color(Color),
    /// `font`.
    Font(Font),
    /// `Text.elide`.
    Elide(Elide),
    /// `Text.horizontalAlignment`.
    HAlign(HAlign),
    /// `Text.verticalAlignment`.
    VAlign(VAlign),
}

/// A Rust type a property is read and written as.
pub trait PropType: Sized + Clone + 'static {
    /// As a value.
    fn into_value(self) -> Value;
    /// From a value, when it holds this type.
    fn from_value(value: &Value) -> Option<Self>;
    /// What a property of this type reads as before it has a value.
    fn fallback() -> Self;
}

/// [`PropType`] for a type held directly in one variant.
macro_rules! prop_type {
    ($type:ty, $variant:ident, $fallback:expr) => {
        impl PropType for $type {
            fn into_value(self) -> Value {
                Value::$variant(self)
            }
            fn from_value(value: &Value) -> Option<Self> {
                match value {
                    Value::$variant(inner) => Some(inner.clone()),
                    _ => None,
                }
            }
            fn fallback() -> Self {
                $fallback
            }
        }
    };
}

prop_type!(bool, Bool, false);
prop_type!(i32, Int, 0);
prop_type!(f32, Real, 0.0);
prop_type!(Rc<str>, Str, Rc::from(""));
prop_type!(Color, Color, Color::TRANSPARENT);
prop_type!(Font, Font, Font::default());
prop_type!(Elide, Elide, Elide::None);
prop_type!(HAlign, HAlign, HAlign::Left);
prop_type!(VAlign, VAlign, VAlign::Center);
