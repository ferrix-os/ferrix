# tole, the UI framework

Ferrix's own framework for the windows its apps draw: a declarative tree of
items with typed properties and bindings, written in a QML-like markup and
compiled into Rust when the app is built. It is modelled on Qt Quick and QML
and draws through the desktop's own client libraries, `compositor-toolkit`
and `compositor-text`, on tiny-skia, on the CPU.

It is called **tole** (customer, 2026-10-23; §6): the crates are `tole`
and `tole-compiler`, and the markup files end in `.tole`.

Outside the certified item: everything here is ring 3, in the compositor's
workspace, and reaches the kernel only through the Linux ABI.

Status: designed 2026-10-23, and being built on the recommended compile
model; the customer answered the design's open questions the same day
(§8). §8 says where it stands.

---

## 1. Why, and what it replaces

Every graphical program Ferrix writes itself draws by hand today. The lode
viewer (an app, `docs/APPS.md`) is the largest: 6,400 lines, of which
about 2,700 are drawing -- `draw.rs`'s `Canvas` (rounded boxes, outlines,
dashes, hatching, text in two faces), `ui/mod.rs`, `ui/logs.rs` and
`ui/overview.rs`, which place every box in logical pixels, and `window.rs`,
which joins them to a Wayland toplevel. Its widgets are each written once,
in place: a virtualized table with rows of six kinds, chips, facets with
checkboxes, a query field, segmented buttons, a side navigation, a detail
pane, charts and sparklines, a palette in light and dark. Settings
(also an app, 2,600 lines) has its own `layout.rs`, `paint/`
and `theme.rs` doing the same things a second time, and the desktop clients
(notifyd, wallpaper, caption, vptr) a third.

Drawing whole frames from the state, immediately, was the right size for
the first windows. It stops being right where these programs now are:

* **Layout is arithmetic in the drawing code.** `ui/logs.rs`'s
  `table_area` repeats the sum of every height above the table
  (`HEADER + 28 + 54 + TRACK + 20 + 42 + 14 + 26 + 16`) so the window can
  scroll it, and a change to one block's height is a change in two places.
* **Every frame is drawn whole.** A hover, a caret blink, one new record in
  Live repaints 1.6 million pixels and reshapes the visible text (the run
  cache hides most of the shaping).
* **Input is a list of rectangles the last frame left** (`Canvas::hit`), so
  there is no focus, no keyboard navigation, no hover state, and no place
  for accessibility to attach.
* **Nothing is shared.** A checkbox in Settings and one in the viewer are
  two pieces of code with two looks.

The framework keeps what made those programs small and quick -- static
binaries, CPU drawing, the desktop's own text and Wayland code, no
interpreter -- and gives them a retained tree to say all of this once.

## 2. What it takes from Qt and QML, and what it leaves

### 2.1 Taken

| Qt / QML | Here | Notes |
|---|---|---|
| A declarative tree of items with typed properties | The same: `Item`, `Rectangle`, `Text`, `Image`, ... each with typed properties | Types are checked when the app is built, never at run time |
| Property bindings with dependency tracking | The same: `width: parent.width - 20` re-evaluates when `parent.width` changes | Dependencies are recorded as a binding reads (§4.1), as `QProperty` does |
| `id:`, `parent`, scoping by id | The same | |
| Signals, `onX:` handlers, `onXChanged:` | The same | A handler is a few statements of the expression language, or a call into Rust |
| Anchors | `anchors.fill`, `centerIn`, `left/right/top/bottom/horizontalCenter/verticalCenter`, margins | Resolved as bindings on `x`, `y`, `width`, `height` |
| Positioners and layouts | `Row`, `Column`, `Grid`, `Flow`, and `RowLayout`/`ColumnLayout`/`GridLayout` with `Layout.fillWidth`, `preferredWidth`, `minimum*`, `maximum*`, margins, alignment | One layout pass (§4.3) |
| `Repeater`, `ListView`, `TableView`, `GridView` with models and delegates | The same, with delegates created only for what is in view and recycled (`reuseItems`) | Models are Rust (§3.4) |
| `DelegateChooser` | The same | The viewer's table has six kinds of row |
| States and transitions | `states: [State { when: ...; PropertyChanges { ... } }]`, `transitions` | |
| Animations | `NumberAnimation`, `ColorAnimation`, `PauseAnimation`, `SequentialAnimation`, `ParallelAnimation`, `Behavior on x`, Qt's easing curves | Driven by the surface's frame callbacks |
| Components and reuse | One component per file, inline `component Name : Base { }`, `required property`, `default property` and children | |
| Focus and keyboard navigation | `focus`, `activeFocus`, `FocusScope`, `activeFocusOnTab`, `KeyNavigation`, `Keys.onPressed` | |
| Pointer handling | `MouseArea` (pressed, hover, wheel, drag), `HoverHandler`, `TapHandler` | |
| Qt Quick Controls and their styles | `Fx.Controls`: `Button`, `ToolButton`, `CheckBox`, `RadioButton`, `Switch`, `TextField`, `TextArea`, `ComboBox`, `SpinBox`, `Slider`, `ProgressBar`, `ScrollView`, `ScrollBar`, `SplitView`, `TabBar`/`TabButton`, `ButtonGroup` (segmented), `Popup`, `Menu`, `ToolTip`, `Label` | Written in the markup itself on the primitives, as Qt Quick Controls are in QML; restyled through `Theme` (§2.3) |
| `Canvas`, `Shape` | `Shape { ShapePath { ... } }` for fixed figures, and `Canvas` whose painting is a Rust function (§3.5) | Charts and sparklines are Rust painters over the same renderer |
| `Loader`, `StackView`-like page switching | `Loader { sourceComponent: ... }`, `StackLayout` | |
| Singletons (`pragma Singleton`) | `singleton Theme { ... }` | The palette and app-wide state |
| Attached properties | `Layout.*`, `ListView.isCurrentItem`, `Keys.*`, `Accessible.*` | Only the framework's own; a program cannot add kinds of attached property |

### 2.2 Left out, on purpose

* **JavaScript.** No interpreter, no `eval`, no dynamic objects. Bindings
  and handlers are a small typed expression language (§3.2): arithmetic,
  comparisons, `? :`, string building, property and model-role reads,
  calls of declared functions and of Rust callbacks, assignment and `if` in
  handlers. Loops, collections and anything with a lifetime of its own stay
  in Rust, where they are tested.
* **Dynamic typing.** `var` does not exist. A property has one type, known
  at build time.
* **`Qt.createComponent`, `createObject` and the network.** A tree's shape
  changes only through `Loader`, `Repeater`, the views and `visible`.
* **The scene-graph's GPU-only parts:** shader effects, `layer.effect`,
  particles, 3D, multimedia items, WebView. The renderer trait (§4.6) keeps
  the door open for a GPU path; nothing in the language depends on one.
* **Qt's C++ object model.** No `QObject`, no meta-object compiler, no
  moc. The compiler generates plain Rust structs; Rust code owns them.
* **Platform plugins.** One backend family: Wayland through
  `compositor-toolkit`, and headless (PNG) for tests.
* **Text documents, rich text beyond `compositor-text`'s Pango subset, and
  bidirectional editing.** The viewer and Settings need neither yet.
* **Translations (`qsTr`).** Later, when Ferrix has a second language.

### 2.3 The theme

Qt Quick Controls separates the controls from their style (Basic, Material,
Universal) and lets a program pick a palette (`Material.theme: Dark`).
Here the style is the one `Fx.Controls` ships, read entirely from a
`Theme` singleton of typed tokens: colours (`bg`, `bg2`, `ink`, `ink2`,
`line`, `accent`, `danger`, ... in light and dark), radii, spacings, and
the two faces (`Theme.sans(px, weight)`, `Theme.mono(px, weight)`). An app
gives its own token values -- the viewer's `theme.rs` palette becomes its
`Theme` -- and switches between light and dark by setting one property,
`Theme.dark`, whose change re-evaluates every binding that read a token and
repaints only what changed colour. There is one style (customer,
2026-10-23): the viewer, Settings and the desktop clients all take
`Fx.Controls`' look, and differ only by their tokens -- light or dark, and
an accent colour.

## 3. The language

### 3.1 Files and shape

A file ends in `.tole` and holds one exported component and any number of
`struct`, `enum` and inline `component` declarations. It reads as QML:

```qml
import Fx
import Fx.Controls

component Chip : Rectangle {
    property string text
    property bool removable: false
    signal removed()

    height: 26
    width: label.implicitWidth + (removable ? 36 : 22)
    radius: 13
    color: Theme.bg
    border { width: 1; color: Theme.line3 }

    Text {
        id: label
        x: 11
        anchors.verticalCenter: parent.verticalCenter
        text: parent.text
        font: Theme.sans(12)
        color: Theme.ink
    }
    Text {
        visible: removable
        anchors { right: parent.right; rightMargin: 11; verticalCenter: parent.verticalCenter }
        text: "×"
        color: Theme.ink4
    }
    MouseArea {
        anchors.fill: parent
        enabled: removable
        onClicked: removed()
    }
}
```

What differs from QML, and why:

* **`struct` and `enum`** are declared in the markup. A model's rows and a
  Rust call's arguments need a type both sides agree on; QML gets it from
  C++ at run time, here the compiler generates the Rust type from the
  declaration, so the agreement is checked by `rustc`.
* **Property types are Rust-sized:** `bool`, `int` (`i32`), `real` (`f32`),
  `string`, `color`, `font`, `length` (logical pixels, `f32`), `duration`,
  `image`, enums, structs, `list<T>` (a value list, small), `model<T>` (a
  Rust model, any size, §3.4), `painter` (a Rust paint function, §3.5).
* **Handlers are statements, not JavaScript**: `a = b`, `call(x)`,
  `if (c) { ... } else { ... }`, `return`. No `for`, no `var`.
* **Functions are typed and pure**: `function label(level: int): string {
  return ... }`, which bindings may call and the dependency tracker sees
  through.

### 3.2 Expressions

Literals (`12`, `0.5`, `"text"`, `#e6e6e2`, `#2a62c91a`, `120ms`,
`true`), property paths (`parent.width`, `table.contentY`,
`modelData.level`, `Theme.ink`), the operators of C with `? :`,
string `+` and `"...${expr}..."` interpolation, enum values
(`LineKind.Fold`), calls of the framework's built-ins (`min`, `max`,
`clamp`, `round`, `floor`, `abs`, `Qt.rgba`, `Qt.lighter`, `Qt.darker`,
`group(n)` for digit grouping, `formatReal`), of the file's own functions
and of Rust functions the app declares (§3.4). Types are inferred bottom
up and checked against the property they are bound to; `int` widens to
`real` and `length`, nothing narrows without `round()`.

### 3.3 How it is compiled

**Recommended, and what is built: at build time, into Rust.** An app's
`build.rs` calls `tole_compiler::build("ui")`, which reads every `.tole`
file under that directory, resolves imports (the framework's own `Fx` and
`Fx.Controls` modules are `.tole` files inside the compiler crate, compiled
the same way), type-checks the whole set and writes one Rust file to
`OUT_DIR`. The app includes it with `tole::include_ui!()`. The result:

* **No interpreter in the binary.** A component becomes a Rust struct with
  a `new` that builds its subtree, each binding a closure, each handler a
  function. What the binary carries is the runtime (§4) plus the generated
  code, which is about the size of the hand-written drawing it replaces.
* **Errors at build time,** with the file, line and column of the markup:
  an unknown property, a type mismatch, an id used outside its scope, a
  binding loop the compiler can see statically.
* **Rust checks the joints.** The generated setters, getters and callbacks
  are typed Rust; an app that passes the wrong row type does not compile.

This is Slint's approach, and the reasons are the same: a static, small,
checked binary and no run-time parse. The compiler is pure Rust (a
hand-written lexer and recursive-descent parser, a type checker, a code
generator printing Rust source), host-tested with golden outputs; it adds
nothing to the app's binary, being a build dependency.

**For development: a run-time loader, later.** `tole-live`, a host-only
crate behind a cargo feature, parses and checks the same files with the
same compiler front end and evaluates the tree directly over the same
runtime, watching the files with inotify and rebuilding the tree on save,
with the Rust-side state kept. The generated interface (setters,
callbacks) is the same trait either way, so an app switches with a feature
flag and its Rust does not change. It never ships in an image. It is the
last step of the plan (§7), after the two consumers are ported.

### 3.4 Rust's side: properties, callbacks, models

The root component's properties are its interface. For

```qml
component LogsPage : Item {
    property string query
    property model<LogLine> lines
    property int selected: -1
    signal lineActivated(int row)
    ...
}
```

the compiler generates:

```rust
pub struct LogsPage { /* the item tree's handle */ }
impl LogsPage {
    pub fn new(ui: &mut tole::Ui<S>) -> Self;
    pub fn set_query(&self, value: String);
    pub fn query(&self) -> String;
    pub fn set_lines(&self, model: tole::ModelRc<LogLine>);
    pub fn set_selected(&self, value: i32);
    pub fn on_line_activated(&self, f: impl Fn(&mut S, i32) + 'static);
}
#[derive(Clone, Debug, PartialEq)]
pub struct LogLine { pub kind: LineKind, pub level: i32, pub time: String, ... }
```

`S` is the program's own state type. Callbacks are handed `&mut S`
when the event loop runs them, so the program's state needs no `RefCell`
and no channel, which keeps the toolkit's "pull, not callbacks" shape on
the program's side: the loop owns the state and lends it.

**Models** are Rust types implementing `tole::Model`:

```rust
pub trait Model {
    type Row;
    fn row_count(&self) -> usize;
    fn row(&self, index: usize) -> Option<Self::Row>;
    /// A row's height when the view needs it without making the row:
    /// variable-height rows in a long list (the viewer's folds and notes).
    fn row_height(&self, index: usize) -> Option<f32> { None }
    fn tracker(&self) -> &tole::ModelTracker; // says rows changed, came, went, or all of them
}
```

`tole::VecModel<T>` covers the small cases; the viewer's lines are a model
over its own `Vec<Line>` and the log, producing a `LogLine` only for rows in
view, so a million records cost nothing until scrolled to.

**Globals.** A `singleton` declared in markup is reachable from Rust the
same way (`ui.global::<Theme>().set_dark(true)`), and Rust functions an app
wants callable from bindings are declared as `native function
clock(us: int, zone: int): string` and given with `ui.set_native_clock(...)`.

### 3.5 Painters

`Canvas` has a `painter` property: a Rust closure given a `Painter` (the
renderer's drawing calls, in the item's own logical coordinates) and the
item's size. It is repainted when the item resizes or when Rust says its
data changed (`canvas.update()`). The viewer's swim lanes, activity bars,
sparklines and minimap are painters: they are loops over data, which is
Rust's work, and `Shape` covers fixed figures (the boot picker's triangle,
the skip marks).

### 3.6 The worked example: the viewer's Logs page

The page as `ui/logs.rs` draws it (§1), in the markup. `PageHeading`,
`Track`, `Badge`, `Chip` and `FacetList` are the viewer's own components in
files beside it; the Overview reuses `PageHeading` and `Track`.

```qml
// logs.tole: the query, its chips and count, the facets, the table, the
// strip showing where the problems are, and a record's every field.
import Fx
import Fx.Controls

enum LineKind { Record, Note, Held, Detail, Fold, Mark }

struct LogLine {
    row: int            // the line's index, for the callbacks
    kind: LineKind
    level: int
    time: string
    source: string
    message: string
    action: string      // "Expand" or "Fold" on held and folded lines
}

struct Facet { name: string; count: int; on: bool }
struct ChipItem { label: string; removable: bool }

export component LogsPage : Item {
    // What Rust gives the page.
    property string query
    property string queryError
    property string savedLabel
    property bool edited
    property list<ChipItem> chips
    property string suggestion          // "+ not hyprix", or empty
    property string count               // "1,204 of 18,330 records"
    property list<Facet> levels
    property list<Facet> sources
    property int moreSources
    property model<LogLine> lines
    property int selected: -1
    property bool live
    property painter minimap
    property bool detailOpen: selected >= 0

    // What the page tells Rust.
    signal save()
    signal queryAccepted(string text)
    signal removeChip(int index)
    signal addSuggestion()
    signal levelToggled(int level)
    signal sourceToggled(int index)
    signal lineActivated(int row)
    signal expand(int row)

    RowLayout {
        anchors.fill: parent
        spacing: 0

        ColumnLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            Layout.margins: Theme.margin           // 36
            Layout.topMargin: 28
            spacing: 0

            PageHeading {
                title: "Logs"
                Layout.fillWidth: true
                Button {
                    text: "Save"
                    highlighted: edited
                    enabled: edited
                    onClicked: save()
                }
            }

            Track { Layout.fillWidth: true; Layout.topMargin: 24 }

            TextField {
                Layout.fillWidth: true
                Layout.topMargin: 20
                Layout.preferredHeight: 42
                font: Theme.mono(13)
                leading: "where"
                trailing: savedLabel
                text: query
                placeholderText: "lode's filter language: priority <= warning or id = \"hyprix*\""
                error: queryError != ""
                onAccepted: queryAccepted(text)
            }
            Label {
                visible: queryError != ""
                text: "does not parse: ${queryError}"
                color: Theme.redInk
                font: Theme.sans(11.5)
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.topMargin: 14
                Flow {
                    Layout.fillWidth: true
                    spacing: 8
                    Repeater {
                        model: chips
                        Chip {
                            required property ChipItem modelData
                            required property int index
                            text: modelData.label
                            removable: modelData.removable
                            onRemoved: removeChip(index)
                        }
                    }
                    Chip {
                        visible: suggestion != ""
                        text: suggestion
                        dashed: true
                        onClicked: addSuggestion()
                    }
                }
                Label { text: count; color: Theme.ink4; font: Theme.sans(12) }
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.fillHeight: true
                Layout.topMargin: 16
                spacing: 16

                Column {
                    Layout.preferredWidth: 180
                    Layout.alignment: Qt.AlignTop
                    spacing: 16
                    FacetList {
                        title: "Priority"
                        checkable: true
                        facets: levels
                        onToggled: (index) => levelToggled(index)
                    }
                    FacetList {
                        title: "Source"
                        facets: sources
                        more: moreSources
                        onToggled: (index) => sourceToggled(index)
                    }
                }

                Rectangle {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    radius: 8
                    color: Theme.bg
                    border { width: 1; color: Theme.line }
                    clip: true

                    Row {
                        id: head
                        height: 30
                        Repeater {
                            model: [["TIME", 42], ["SOURCE", 104], ["MESSAGE", 100]]
                            Label {
                                required property list<string> modelData
                                leftPadding: modelData[1]
                                height: head.height
                                text: modelData[0]
                                font: Theme.sans(11, 600)
                                color: Theme.ink4
                            }
                        }
                    }

                    ListView {
                        id: table
                        anchors { fill: parent; topMargin: 31 }
                        model: lines
                        reuseItems: true
                        footer: Label {
                            text: live ? "— end of window · following —"
                                 : table.count == 0 ? "— nothing matches —" : "— end of window —"
                            font: Theme.mono(11.5)
                            color: Theme.ink4
                            leftPadding: 10
                        }
                        delegate: DelegateChooser {
                            role: "kind"
                            DelegateChoice { roleValue: LineKind.Record; RecordRow { } }
                            DelegateChoice { roleValue: LineKind.Held; HeldRow { onExpand: expand(line.row) } }
                            DelegateChoice { roleValue: LineKind.Fold; FoldRow { onExpand: expand(line.row) } }
                            DelegateChoice { roleValue: LineKind.Note; NoteRow { } }
                            DelegateChoice { roleValue: LineKind.Detail; DetailRow { } }
                            DelegateChoice { roleValue: LineKind.Mark; MarkRow { } }
                        }
                    }
                }

                Canvas {
                    Layout.preferredWidth: 12
                    Layout.fillHeight: true
                    painter: minimap
                }
            }
        }

        RecordPane {
            visible: detailOpen
            Layout.preferredWidth: 392
            Layout.fillHeight: true
            record: selected
        }
    }

    component RecordRow : Rectangle {
        required property LogLine modelData
        property LogLine line: modelData
        width: ListView.view.width
        height: 28
        color: line.level <= 3 ? Theme.redBg : line.level == 4 ? Theme.amberBg : "transparent"
        border { width: ListView.isCurrentItem ? 1.5 : 0; color: Theme.ink }
        radius: 2

        Badge { x: 12; y: 6; level: line.level; kind: line.kind }
        Label { x: 42; height: parent.height; text: line.time; font: Theme.mono(12.5); color: Theme.ink4 }
        Label { x: 146; width: 92; height: parent.height; text: line.source; elide: Text.ElideRight
                font: Theme.mono(12.5) }
        Label { x: 246; width: parent.width - 256; height: parent.height; text: line.message
                elide: Text.ElideRight; font: Theme.mono(12.5) }
        MouseArea { anchors.fill: parent; onClicked: lineActivated(line.row) }
    }

    // HeldRow, FoldRow, NoteRow, DetailRow and MarkRow are written the same
    // way, each a dozen lines.
}
```

and the Rust side, replacing `ui/logs.rs`'s 616 lines with what the page
cannot know:

```rust
tole::include_ui!();                       // LogsPage, LogLine, LineKind, ...

let page = LogsPage::new(&mut ui);
page.set_lines(ModelRc::new(LinesModel::new(&app)));     // impl Model<Row = LogLine>
page.set_minimap(painter(|app: &App, p: &mut Painter, size| minimap(app, p, size)));
page.on_line_activated(|app: &mut App, row| app.select_line(row));
page.on_query_accepted(|app: &mut App, text| app.set_query(&text));
// After each change of state, the parts of it the page shows:
app.show(&page);   // set_query, set_chips, set_count, set_levels, ..., lines.reset()
```

What moves where: layout, colour, hover and focus go to the markup; the
query, the facets' counts, the lines, folding and the painters stay in Rust,
unchanged and still host-tested.

## 4. Architecture

```
  .tole files ──tole-compiler (build.rs)──▶ generated Rust: components, structs, bindings
                                                │
  app state S ◀── callbacks (&mut S) ──┐        ▼
                                       │   tole runtime
                                       │   ├ properties + bindings (dirty propagation)
                                       │   ├ item tree (arena of items)
                                       │   ├ layout pass ──▶ geometry
                                       │   ├ input: hit test, focus, keys, pointer grabs
                                       │   ├ animations (frame clock)
                                       │   └ paint: damage ──▶ Renderer
                                       │                     ├ CPU: tiny-skia + compositor-text
                                       │                     └ (later: GPU)
                                       │   window host
                                       └── ├ Wayland: compositor-toolkit (Ferrix, any Linux desktop)
                                           └ headless: a PNG, for tests and --render
```

### 4.1 Properties and bindings

A property is a cell in the item arena: its value, an optional binding (a
generated closure), a dirty flag, and the list of bindings that read it.
Evaluating a binding sets the arena's "current binding"; every property
read while it is set records the edge. Setting a property marks its
readers dirty, recursively, and does no evaluation: a dirty binding is
evaluated the next time it is read, or in the update pass before layout,
so a burst of changes costs one evaluation. Assigning to a bound property
from a handler removes the binding, as in QML. A cycle found while
evaluating (a binding reached again before it finished) is reported once
with the markup location the compiler stored, and the property keeps its
last value; the compiler rejects the cycles it can see statically.

Each property also has a *role*: geometry (marks the item's parent for
layout), paint (marks the item's painted area as damaged), or neither
(model data, text the item only measures). The generated code knows each
property's role, so a colour change damages without a relayout and a
`width` change relayouts without reshaping text.

Single-threaded, like Qt's GUI thread. No `Send` is needed; no lock is
taken.

### 4.2 The item tree

Items live in a generational arena (`ItemId` = index + generation, so a
stale id from a recycled delegate is refused, never followed). Each has a
kind (the primitives: `Item`, `Rectangle`, `Text`, `TextInput`, `Image`,
`Shape`, `Canvas`, `MouseArea`, the positioners and layouts, `Repeater`,
`Flickable`, the views, `Loader`), its common properties (geometry,
`implicitWidth/Height`, `visible`, `opacity`, `clip`, `z`, `enabled`,
focus) and its kind's own. A component is not an item kind: it is
generated code that builds a subtree of primitives and binds its
properties. `Fx.Controls` are components, so a `Button` costs exactly the
items it draws.

### 4.3 The layout pass

After bindings settle, layout runs from the roots down over items marked
for layout: a positioner or layout reads its children's implicit and
`Layout.*` sizes (bottom up, cached until a child's geometry changes),
then sets their `x`, `y`, `width`, `height` (top down). Anchors are not a
pass of their own: the compiler turns them into bindings on the same
four properties. A view lays out only the delegates in view, from the
model's row heights (exact when `row_height` answers, estimated from the
delegates made so far when it does not, corrected as rows are made, as
`ListView` does).

### 4.4 Damage and partial redraw

Each item remembers the rectangle it last painted, in window
coordinates. A paint-role change damages its old and new rectangles; a
move damages both places; a scroll damages the view. Before a frame the
damage is merged into at most a few rectangles (a union when they overlap
much). The frame paints only the items whose rectangles meet the damage,
in tree order, clipped to the damage, into a retained back buffer the size
of the window; then only the damaged rectangles are copied to the
compositor's buffer and reported with `wl_surface.damage_buffer`.

That needs one addition to `compositor-toolkit`: a `present` that copies
given rectangles from the program's pixmap into the free `wl_shm` slot and
damages only those. Because a slot may hold a frame older than the last
one, the toolkit keeps each slot's age and adds the damage of the frames it
missed (EGL's buffer-age rule), copying the whole frame when the slot is
new. `draw_pixels` already damages rectangles; the new call is its painter
form.

A caret blink is then a 2 × 20 pixel frame, and one new record in Live
the table's area, not the window.

### 4.5 Text

`compositor-text` does it all: font discovery, shaping (rustybuzz), glyph
rasterising with its cache, ellipsizing, line breaking (`Layout`). `Text`
keeps its shaped `Run` or `Layout` until its text, font, width (when it
wraps or elides) or the scale changes; measuring for `implicitWidth` uses
the same run. `TextInput` adds a caret, selection by pointer and keys, the
clipboard through the toolkit, and input methods later
(`zwp_text_input_v3`, when the compositor has it). Fonts load once per
`Ui` (the viewer's `Text::load` becomes the theme's font list).

### 4.6 Rendering

```rust
pub trait Renderer {
    fn begin(&mut self, damage: &[Rect]);
    fn fill_rect(&mut self, rect: Rect, radius: Radii, paint: &Fill);
    fn stroke_rect(&mut self, rect: Rect, radius: Radii, width: f32, color: Color, dash: Option<Dash>);
    fn path(&mut self, path: &Path, fill: Option<&Fill>, stroke: Option<&Stroke>);
    fn text(&mut self, run: &TextRun, origin: Point, color: Color);
    fn image(&mut self, image: &ImageRef, rect: Rect, fit: Fit);
    fn push_clip(&mut self, rect: Rect, radius: Radii);
    fn pop_clip(&mut self);
    fn push_opacity(&mut self, opacity: f32);
    fn pop_opacity(&mut self);
    fn end(&mut self);
}
```

Coordinates are logical pixels with a transform; the renderer scales.
`CpuRenderer` is tiny-skia and `compositor-text`'s glyph masks: the
viewer's `Canvas` reshaped into the trait, with opacity groups as an
offscreen pixmap only when an item with children has `opacity < 1`. A GPU
renderer later implements the same trait over the compositor's render path
(`docs/GPU.md`), and the CPU one stays, always available.

### 4.7 Input

Pointer events go to the topmost enabled `MouseArea`/handler under the
point, in reverse paint order, honouring clips; a press grabs the pointer
until release, as Qt does. Hover is tracked per handler (entered/exited,
`containsMouse`, the cursor shape through `wp_cursor_shape`). Wheel events
go to the innermost `Flickable` or `WheelHandler` that can still move.
Keys go to the item with active focus and bubble to its ancestors through
`Keys.onPressed` until one accepts. Tab and Shift+Tab walk
`activeFocusOnTab` items in tree order within the focus scope.

### 4.8 Accessibility (later)

Every control sets `Accessible.role`, `name` and `description`, and the
tree keeps them now, so nothing in the controls changes when an exporter
is written. The exporter (AT-SPI over D-Bus, on the tree's `dbus` crate)
comes when Ferrix has a screen reader to talk to.

### 4.9 DPI and scaling

Everything is in logical pixels. The window host gives the scale the
compositor asks for (`wl_surface.preferred_buffer_scale`, an integer
today), the renderer draws at it, and text is shaped at the buffer scale so
glyphs are sharp. Fractional scaling (`wp_fractional_scale_v1` with
`wp_viewporter`) needs both in the toolkit first; the framework already
takes a real-valued scale.

### 4.10 Window hosts

* **Wayland, through `compositor-toolkit`:** a toplevel (and popups for
  menus and combo boxes that leave the window), the seat, the cursor, the
  clipboard, timers for animations and Live, the frame callback as the
  animation clock. The same code runs on Ferrix under hyprix and on a
  Linux development host on any Wayland desktop (`WAYLAND_DISPLAY`): the
  toolkit is pure Rust over the socket, no libwayland.
* **Headless:** the tree laid out and painted at a given size and scale
  into a PNG, with synthetic input. It is what the host tests and the
  screenshot comparisons use, and an app's `--render`.

### 4.11 Size

The viewer is 2.6 MB stripped, most of it tiny-skia, `compositor-text`,
rustybuzz and std, which the framework already uses. The runtime adds the
arena, bindings, layout, input and the controls' generated code; the
target is under 400 KB on x86-64 for a program like the viewer, measured at
each step (§7) with a `tole-size` example whose stripped size the host
test records. The compiler is a build dependency and costs the binary
nothing.

### 4.12 Licences

Nothing new: tiny-skia (BSD-3-Clause), rustybuzz and ttf-parser (MIT or
Apache-2.0), all already in the tree. The framework itself is MIT, as the
compositor's workspace. No crate is added.

### 4.13 Where it lives

tole is a repository of its own, https://github.com/ferrix-os/tole
(customer, 2026-10-23), and a component of this tree (`components.toml`),
checked out at `src/user/system/linux/tole`: a workspace of the crates
`tole` (runtime) and `tole-compiler` (compiler and the `Fx` and
`Fx.Controls` modules), with the compositor's lints. It is not in the
compositor's workspace but beside it, and reaches two of its crates by
path: `text` (fonts, shaping, glyphs), which the runtime draws with, and
`toolkit` (the Wayland client runtime), which only the window backend
behind the `wayland` feature uses. Both stay in this tree, where the
compositor's own clients use them; moving them would make every client of
the compositor depend on a component. tole builds alone wherever that
layout is -- its CI checks out the compositor's directory and the tree's
Inter beside it -- and `cargo xtask check` runs its formatting, clippy and
tests as one of the Linux workspaces. Apps reach it by relative path, as
they reach the toolkit. A change to tole is two landings, as for every
component (`docs/CONVENTIONS.md`).

## 5. Testing

* **Host tests** for every part: bindings and dirty propagation, layouts
  against Qt's results for the same input, the compiler's parser, checker
  and generated code (golden files), damage rectangles, hit testing and
  focus order, model views (rows made, recycled and dropped while
  scrolling).
* **Pixel tests:** headless frames compared with PNGs kept beside the
  tests, exact on the CPU renderer (it is deterministic on one machine and
  across x86-64 and AArch64 with tiny-skia's integer pipeline; where it is
  not, the test says why and allows a stated per-channel difference).
* **The ports:** the viewer's `--render` at three sizes, both palettes and
  both pages, before and after its port: the same layout and colours, with
  a small stated antialiasing tolerance (customer, 2026-10-23) -- each
  channel within a few levels, and only on pixels that an edge or a glyph
  partly covers. Settings the same.
* **On Ferrix:** the viewer's existing `test-compositor` row starts it
  under hyprix and screenshots it; after the port that row runs the
  framework's Wayland host end to end.

## 6. Name

Short, typeable, a word, like lode. Checked as lode was
(`docs/backlog/DECISIONS.md`, 2026-10-23): crates.io, Arch, the AUR,
Homebrew, Ubuntu, npm, PyPI, GitHub's best-known repositories, commands on
the development host, and this tree. Every short word is taken on
crates.io by something, so the crates would be `ferrix-<name>` as lode's
is; what matters is whether a well-known project, a command or another UI
library already answers to it.

| Name | Meaning | crates.io | Distributions, commands | npm / PyPI | GitHub, best known | Ferrix |
|---|---|---|---|---|---|---|
| **tole** | Painted sheet metal (French *tôle*): the painted surface over iron | free | none (Ubuntu, Arch, AUR, Homebrew); no command | free / free | nothing named tole (look-alikes: tolerant-php-parser) | unused |
| **vein** | Where the ore runs: lode's sibling, the way you see into what lode keeps | `vein`, a RubyGems proxy, 353 downloads | none; no command | taken, small / taken, small | veinmind-tools (1.6k ★, container security), vector-vein | only in lode's own name note |
| **sinter** | Powder fused into one solid part by heat: markup fused into the binary at build time | `sinter`, a string interner, 2.9k downloads | none; no command | free / taken, small | trailofbits/sinter (300 ★, a macOS endpoint agent, archived) | unused |

Dropped after checking: *mica* (Windows 11's Mica material, a UI term, and
an 11k-download crate), *bezel* (a gpui component library, updated today),
*facet* (a widely used Rust reflection crate), *slate* (Unreal's UI
framework and Slate.js), *ember* (Ember.js), *quill* (the Quill editor),
*niello* (free everywhere, but six letters few can spell), *rivet*, *ingot*,
*etch*, *glint* (an AUR package), *temper* (AUR packages), *anvil* and
*cast* (Foundry's commands).

Recommended and chosen (customer, 2026-10-23): **tole**: free everywhere
checked, four letters, and in the family of lode, ferrous and zinc.

## 7. Plan

Points as the backlog counts them; each step lands on its own, gated by
`cargo xtask check` (the compositor workspace's tests) unless it says
more.

| Step | What | Points | Landable when |
|---|---|---|---|
| **U1** | Runtime core: arena, properties and bindings with dependency tracking, `Item`/`Rectangle`/`Text`, anchors as bindings, `Row`/`Column`, the `Renderer` trait and the CPU renderer, headless PNG. Built from Rust (a builder API the compiler will target). | 8 | host tests: bindings, layout, a pixel test |
| **U2** | Compiler: lexer, parser, type checker, Rust code generation, `build.rs` entry, `include_ui!`; structs, enums, signals, handlers, functions. | 8 | golden tests; an example app built from markup renders the U1 pixel test identically |
| **U3** | Input and the window: hit testing, `MouseArea`, hover, wheel, focus, keys, Tab order; the Wayland host on the toolkit; damage tracking and the toolkit's damaged present. | 8 | host tests with synthetic input; the example runs on a Linux Wayland desktop and under hyprix (`test-compositor` row, where the example joins the image) |
| **U4** | Layouts and views: `RowLayout`/`ColumnLayout`/`GridLayout`/`Flow`/`Grid`, `Repeater`, `Flickable`, `ListView` with virtualization and recycling, `DelegateChooser`, `Model`, `VecModel`. | 8 | host tests: 10⁶ rows scroll with at most a screenful of delegates |
| **U5** | States, transitions, animations, `Behavior`, the frame clock. | 5 | host tests with a fake clock |
| **U6** | `Fx.Controls` and `Theme`: the controls of §2.1, `TextField` editing with the clipboard, `Popup`/`Menu`/`ComboBox`. | 8 | pixel tests of each control in both palettes |
| **U7** | `Shape`, `Canvas` with Rust painters. | 3 | pixel tests |
| **U8** | The lode viewer ported: its pages in markup, its painters in Rust, `--render` identical before and after. | 8 | the screenshot comparison; the viewer's `test-compositor` row |
| **U9** | Settings ported. | 5 | Settings' screenshots compared the same way |
| **U10** | `tole-live`: the run-time loader for hot reload on the host. | 5 | host test: a file edited, the tree rebuilt, the state kept |
| later | GPU renderer; fractional scale; accessibility export; input methods; the desktop clients (notifyd, caption) where they gain from it. | | |

## 8. Where it stands

* 2026-10-23: designed. The customer said to build on the recommended
  model without waiting for a review, and answered the open questions:
  1. **The name is tole**, the markup's extension `.tole`.
  2. **`struct` and `enum` are declared in the markup** for typed models.
  3. **"Identical screenshots"** means the same layout and colours, within a
     small stated antialiasing tolerance (§5).
  4. **One style:** Settings and the desktop clients take tole's
     `Fx.Controls` look; theme tokens give light and dark and the accents.
* **U1 built** (2026-10-23), landed as 91eb77d24 in the compositor's
  workspace, then moved with its history to ferrix-os/tole (§4.13), pinned
  here. The
  property cells and bindings with dependency tracking (lazy: a change
  marks, a read evaluates; dropped dependencies forgotten; loops found and
  said once), the item arena with generations, `Item`, `Rectangle`,
  `Text`, `Row` and `Column` as bindings, anchors as bindings, the scene
  as drawing operations, the `Renderer` trait, the CPU renderer with
  clips (a mask only when a shape crosses one) and opacity groups, and a
  PNG writer. `compositor-text` gained `Fonts::draw_run_clipped`, so text
  stops at a clip without a layer. 15 host tests.
* The lode viewer, the first consumer, is being landed by another session;
  its port (U8) starts once it is on `develop`.
* **U2 built** (2026-10-23): `tole-compiler`, in ferrix-os/tole. `.tole`
  files lexed, parsed, gathered and type-checked, then written out as one
  Rust module for `tole::include_ui!()` from a `build.rs`; errors with
  file, line and column. It knows `struct`, `enum`, components on a kind
  or a component, children, ids, `parent`, declared properties, bindings
  on any property and grouped ones, anchors, typed functions, and the
  expression language of §3.2 but for calls into Rust. A constant is set
  once, anything that reads a property bound. Signals and handlers come
  with input (U3). Six host tests; and the `tole-examples` crate builds
  real `.tole` programs and draws them.
* **Demos** (the customer's ask, 2026-10-23): `examples/` in ferrix-os/tole,
  each `.tole` program drawn headless at scale 2 into
  `docs/screenshots/`, shown in its README beside its markup; a test fails
  when a picture is stale. hello and layouts so far; controls, a 100,000-row
  list, states, the theme, a chart and the ported viewer follow their
  steps. The PNG writer deflates with `miniz_oxide`.
* Differences from §3.4 found while building: the generated API takes the
  `Ui` (`page.set(&mut ui, ...)`, `Prop::set`) rather than holding it, so
  a program's state and its tree stay two plain values.
* **U3a built** (2026-10-23): signals and handlers, `MouseArea`, the
  pointer. A signal is an id in the `Ui`; markup handlers are closures
  connected to it, and every emission is queued for the program, which
  reads it through an enum the compiler writes per component
  (`button.signal(&emitted) == Some(ButtonSignal::Clicked)`) -- the
  program's Rust is not called back, which replaces §3.4's `on_x(|app:
  &mut App, ..|)` callbacks: pull, as `compositor-toolkit`'s events are,
  so the program's state needs no `RefCell` and the `Ui` no type
  parameter. Handler bodies: assignments, emissions, calls, `if`/`else`;
  a handler emitting the signal it handles is refused at build time. The
  pointer: topmost enabled area in paint order inside its clips, a press
  held to its release, hover with entered and exited. Declared properties
  and signals are freed with their component's root. A `counter` demo.
  33 host tests in all.
* **U3b built** (2026-10-23): damage and the window. Each item's drawing
  is compared with the last frame's; the damage, merged into at most
  eight rectangles, is drawn into a kept `Frame` and handed over with the
  toolkit's `draw_damaged` (landed here as 7f86939d6, which also copies
  what a buffer missed while the others were shown). A part is drawn on a
  full-size scratch with only the drawing that reaches it: drawing it on
  a pixmap of its own size or through a mask left edge pixels up to 26
  levels off, because tiny-skia chops a curve where an edge cuts it; the
  scratch keeps the kept frame equal to a whole one, pixel for pixel, as
  a test checks at scales 1, 1.5 and 2. `tole::window::Window` (feature
  `wayland`) opens a toplevel, feeds the pointer, sizes the root, follows
  the scale and paces draws by frame callbacks; checked by hand under a
  headless hyprix. The demos' binary is 2.6 MB stripped: the order of
  the viewer's, as §4.11 wanted.
* Next: U3c, focus, keys and the Tab order; then U4, the views and
  layouts the lode viewer's table needs. The window gets a
  `test-compositor` row with the viewer's port (U8), where it first runs
  on Ferrix.
