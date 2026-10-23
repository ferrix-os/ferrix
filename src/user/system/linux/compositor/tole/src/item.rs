//! The item tree (`docs/UI-FRAMEWORK.md` §4.2): items in a generational
//! arena, each a kind and its properties, a parent and children.
//!
//! An item is made with [`Ui::create`], which gives every property its
//! default -- `width` and `height` bound to the implicit size, as in QML --
//! and, under a positioner, binds its place. A component of the markup is
//! not a kind: the compiler makes it as a subtree of these.

use std::rc::Rc;

use crate::ui::{Prop, Ui};
use crate::value::{Color, Elide, Font, HAlign, VAlign};

/// An item's id: an index and a generation, so the id of a destroyed item
/// (a recycled delegate's, say) is refused rather than followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ItemId {
    /// The index.
    index: u32,
    /// Its generation.
    generation: u32,
}

/// What every item has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Common {
    /// Left, from the parent's left.
    pub x: Prop<f32>,
    /// Top, from the parent's top.
    pub y: Prop<f32>,
    /// Width; bound to `implicit_width` until set.
    pub width: Prop<f32>,
    /// Height; bound to `implicit_height` until set.
    pub height: Prop<f32>,
    /// The width its content asks for.
    pub implicit_width: Prop<f32>,
    /// The height its content asks for.
    pub implicit_height: Prop<f32>,
    /// Whether it and its children are drawn (and take room in a
    /// positioner).
    pub visible: Prop<bool>,
    /// 0 to 1, for it and its children together.
    pub opacity: Prop<f32>,
    /// Whether its children are cut at its edges.
    pub clip: Prop<bool>,
}

/// A `Rectangle`'s own properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectangleProps {
    /// The fill.
    pub color: Prop<Color>,
    /// The corners' radius.
    pub radius: Prop<f32>,
    /// The border's width, inside the edge; 0 for none.
    pub border_width: Prop<f32>,
    /// The border's colour.
    pub border_color: Prop<Color>,
}

/// A `Text`'s own properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextProps {
    /// The text: one line.
    pub text: Prop<Rc<str>>,
    /// Its colour.
    pub color: Prop<Color>,
    /// Its font.
    pub font: Prop<Font>,
    /// Where it is cut when wider than the item.
    pub elide: Prop<Elide>,
    /// Where it sits across the item.
    pub horizontal_alignment: Prop<HAlign>,
    /// Where it sits down the item.
    pub vertical_alignment: Prop<VAlign>,
}

/// A `Row`'s or `Column`'s own properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionerProps {
    /// Between two children.
    pub spacing: Prop<f32>,
    /// Around them all.
    pub padding: Prop<f32>,
}

/// What an item is, with its own properties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A plain `Item`: geometry and children, nothing drawn.
    Item,
    /// A `Rectangle`.
    Rectangle(RectangleProps),
    /// A `Text`.
    Text(TextProps),
    /// A `Row`: its children side by side.
    Row(PositionerProps),
    /// A `Column`: its children one under another.
    Column(PositionerProps),
}

/// The kinds, to make one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KindName {
    /// [`Kind::Item`].
    Item,
    /// [`Kind::Rectangle`].
    Rectangle,
    /// [`Kind::Text`].
    Text,
    /// [`Kind::Row`].
    Row,
    /// [`Kind::Column`].
    Column,
}

/// One item.
#[derive(Debug)]
pub(crate) struct ItemData {
    /// Its kind and own properties.
    pub(crate) kind: Kind,
    /// Its common properties.
    pub(crate) common: Common,
    /// Its parent.
    pub(crate) parent: Option<ItemId>,
    /// Its children, in paint order.
    pub(crate) children: Vec<ItemId>,
}

/// A place in the arena.
#[derive(Debug)]
pub(crate) struct Slot {
    /// Bumped each time the slot is freed.
    generation: u32,
    /// The item, while it lives.
    pub(crate) item: Option<ItemData>,
}

impl Ui {
    /// A new item of `kind` under `parent`, or a root when `None`. Answers
    /// `None` only when `parent` is gone.
    pub fn create(&mut self, kind: KindName, parent: Option<ItemId>) -> Option<ItemId> {
        if let Some(parent) = parent
            && self.item(parent).is_none()
        {
            return None;
        }
        let common = Common {
            x: self.prop(0.0_f32),
            y: self.prop(0.0_f32),
            width: self.prop(0.0_f32),
            height: self.prop(0.0_f32),
            implicit_width: self.prop(0.0_f32),
            implicit_height: self.prop(0.0_f32),
            visible: self.prop(true),
            opacity: self.prop(1.0_f32),
            clip: self.prop(false),
        };
        let (implicit_width, implicit_height) = (common.implicit_width, common.implicit_height);
        common.width.bind(self, move |ui| ui.get(implicit_width));
        common.height.bind(self, move |ui| ui.get(implicit_height));
        let kind = match kind {
            KindName::Item => Kind::Item,
            KindName::Rectangle => Kind::Rectangle(RectangleProps {
                color: self.prop(Color::hex(0xff_ff_ff)),
                radius: self.prop(0.0_f32),
                border_width: self.prop(0.0_f32),
                border_color: self.prop(Color::hex(0)),
            }),
            KindName::Text => {
                let props = TextProps {
                    text: self.prop(Rc::<str>::from("")),
                    color: self.prop(Color::hex(0)),
                    font: self.prop(Font::default()),
                    elide: self.prop(Elide::None),
                    horizontal_alignment: self.prop(HAlign::Left),
                    vertical_alignment: self.prop(VAlign::Center),
                };
                implicit_width.bind(self, move |ui| {
                    let (text, font, scale) = (ui.get(props.text), ui.get(props.font), ui.scale());
                    ui.text.run(&font, scale, &text).width / scale
                });
                implicit_height.bind(self, move |ui| {
                    let (font, scale) = (ui.get(props.font), ui.scale());
                    let (ascent, descent) = ui.text.extent(&font, scale);
                    (ascent + descent) / scale
                });
                Kind::Text(props)
            }
            KindName::Row | KindName::Column => {
                let props = PositionerProps {
                    spacing: self.prop(0.0_f32),
                    padding: self.prop(0.0_f32),
                };
                if kind == KindName::Row {
                    Kind::Row(props)
                } else {
                    Kind::Column(props)
                }
            }
        };
        let data = ItemData {
            kind,
            common,
            parent,
            children: Vec::new(),
        };
        let id = self.place(data);
        match parent {
            Some(parent) => {
                if let Some(item) = self.item_mut(parent) {
                    item.children.push(id);
                }
                self.position_children(parent);
            }
            None => self.roots.push(id),
        }
        Some(id)
    }

    /// Destroy `item` and everything under it.
    pub fn destroy(&mut self, item: ItemId) {
        let Some(data) = self.item(item) else {
            return;
        };
        let parent = data.parent;
        let mut doomed = vec![item];
        let mut at = 0;
        while let Some(&next) = doomed.get(at) {
            if let Some(data) = self.item(next) {
                doomed.extend(data.children.iter().copied());
            }
            at += 1;
        }
        for id in doomed {
            let Some(data) = self.slot_mut(id).and_then(|slot| {
                slot.generation = slot.generation.wrapping_add(1);
                slot.item.take()
            }) else {
                continue;
            };
            for prop in props_of(&data) {
                self.free(prop);
            }
            self.free_items.push(id.index);
        }
        match parent {
            Some(parent) => {
                if let Some(data) = self.item_mut(parent) {
                    data.children.retain(|&child| child != item);
                }
                self.position_children(parent);
            }
            None => self.roots.retain(|&root| root != item),
        }
    }

    /// Whether `item` lives.
    #[must_use]
    pub fn exists(&self, item: ItemId) -> bool {
        self.item(item).is_some()
    }

    /// `item`'s kind and own properties.
    #[must_use]
    pub fn kind(&self, item: ItemId) -> Option<Kind> {
        self.item(item).map(|data| data.kind)
    }

    /// `item`'s common properties.
    #[must_use]
    pub fn common(&self, item: ItemId) -> Option<Common> {
        self.item(item).map(|data| data.common)
    }

    /// A `Rectangle`'s own properties.
    #[must_use]
    pub fn rectangle(&self, item: ItemId) -> Option<RectangleProps> {
        match self.kind(item)? {
            Kind::Rectangle(props) => Some(props),
            _ => None,
        }
    }

    /// A `Text`'s own properties.
    #[must_use]
    pub fn text(&self, item: ItemId) -> Option<TextProps> {
        match self.kind(item)? {
            Kind::Text(props) => Some(props),
            _ => None,
        }
    }

    /// A `Row`'s or `Column`'s own properties.
    #[must_use]
    pub fn positioner(&self, item: ItemId) -> Option<PositionerProps> {
        match self.kind(item)? {
            Kind::Row(props) | Kind::Column(props) => Some(props),
            _ => None,
        }
    }

    /// `item`'s parent.
    #[must_use]
    pub fn parent(&self, item: ItemId) -> Option<ItemId> {
        self.item(item)?.parent
    }

    /// `item`'s children, in paint order.
    #[must_use]
    pub fn children(&self, item: ItemId) -> &[ItemId] {
        self.item(item).map_or(&[], |data| data.children.as_slice())
    }

    /// The items with no parent.
    #[must_use]
    pub fn roots(&self) -> &[ItemId] {
        &self.roots
    }

    /// The live item `id` names.
    pub(crate) fn item(&self, id: ItemId) -> Option<&ItemData> {
        self.items
            .get(id.index as usize)
            .filter(|slot| slot.generation == id.generation)
            .and_then(|slot| slot.item.as_ref())
    }

    /// The same, to change.
    fn item_mut(&mut self, id: ItemId) -> Option<&mut ItemData> {
        self.items
            .get_mut(id.index as usize)
            .filter(|slot| slot.generation == id.generation)
            .and_then(|slot| slot.item.as_mut())
    }

    /// The slot `id` names, live or not, if its generation matches.
    fn slot_mut(&mut self, id: ItemId) -> Option<&mut Slot> {
        self.items
            .get_mut(id.index as usize)
            .filter(|slot| slot.generation == id.generation)
    }

    /// Put `data` in a free slot.
    fn place(&mut self, data: ItemData) -> ItemId {
        if let Some(index) = self.free_items.pop()
            && let Some(slot) = self.items.get_mut(index as usize)
        {
            slot.item = Some(data);
            return ItemId {
                index,
                generation: slot.generation,
            };
        }
        let index = u32::try_from(self.items.len()).unwrap_or(u32::MAX);
        self.items.push(Slot {
            generation: 0,
            item: Some(data),
        });
        ItemId {
            index,
            generation: 0,
        }
    }
}

/// Every property cell an item holds.
fn props_of(data: &ItemData) -> Vec<crate::ui::PropId> {
    let c = data.common;
    let mut ids = vec![
        c.x.id,
        c.y.id,
        c.width.id,
        c.height.id,
        c.implicit_width.id,
        c.implicit_height.id,
        c.visible.id,
        c.opacity.id,
        c.clip.id,
    ];
    match data.kind {
        Kind::Item => {}
        Kind::Rectangle(p) => ids.extend([
            p.color.id,
            p.radius.id,
            p.border_width.id,
            p.border_color.id,
        ]),
        Kind::Text(p) => ids.extend([
            p.text.id,
            p.color.id,
            p.font.id,
            p.elide.id,
            p.horizontal_alignment.id,
            p.vertical_alignment.id,
        ]),
        Kind::Row(p) | Kind::Column(p) => ids.extend([p.spacing.id, p.padding.id]),
    }
    ids
}
