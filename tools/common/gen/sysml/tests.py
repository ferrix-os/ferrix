#!/usr/bin/env python3
"""Tests for the model reader and the renderers.

The generator is the one thing in this tree that reads a language, and a
reader that quietly misunderstands a construct produces a document that is
wrong rather than a build that fails. So the cases below are mostly about the
*edges* -- where a doc comment attaches, what a value may contain, which
characters a renderer has to defuse -- because those are what a new section or
a new notation will disturb.

The last test is the one that matters most: the real model parses with nothing
left unrecognised. It fails the moment `docs/sysml/` uses something the reader
does not know, which is exactly when the document would start losing content.

Usage:  python3 tools/common/gen/sysml/tests.py
"""

from __future__ import annotations

import importlib.util
import pathlib
import re
import sys
import unittest
import xml.dom.minidom

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent.parent))

from sysml import load, parse_text  # noqa: E402
from sysml import diagrams, document, emit_json, figure, layout  # noqa: E402
from sysml import render_html, render_markdown, render_mermaid, render_svg, sections  # noqa: E402
from sysml.model import humanise  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent.parent.parent


def parse(text: str):
    root, relations, unparsed = parse_text("test.sysml", text)
    return root, relations, unparsed


def only(root):
    """The single package in a parsed fragment."""
    return root.children[0]


class Headers(unittest.TestCase):
    def test_definition_and_usage_are_distinguished(self):
        package = only(parse("package P { part def Engine; part engine : Engine; }")[0])
        definition, usage = package.children
        self.assertTrue(definition.is_definition)
        self.assertEqual(definition.name, "Engine")
        self.assertFalse(usage.is_definition)
        self.assertEqual(usage.name, "engine")
        self.assertEqual(usage.typed_by, "Engine")

    def test_maturity_keyword_and_stage(self):
        package = only(parse("package P { #implemented part x : T { @stage { number = 7; } } }")[0])
        element = package.children[0]
        self.assertEqual(element.maturity, "implemented")
        self.assertEqual(element.stage, 7)

    def test_maturity_is_never_inherited(self):
        package = only(parse("package P { #implemented part def A { part b : B; } }")[0])
        self.assertEqual(package.children[0].maturity, "implemented")
        self.assertEqual(package.children[0].children[0].maturity, "")

    def test_deferred_reason_is_lifted_onto_the_element(self):
        package = only(
            parse('package P { part x : T { @deferred { reason = "QEMU cannot place it."; } } }')[0]
        )
        element = package.children[0]
        self.assertTrue(element.is_deferred)
        self.assertEqual(element.deferred_reason, "QEMU cannot place it.")

    def test_short_name_specialisation_and_redefinition(self):
        package = only(
            parse(
                "package P {"
                " requirement <'G.4'> spawn : GoalRequirement;"
                " part def Sub :> Super;"
                " attribute :>> status = StageStatus::Done;"
                " }"
            )[0]
        )
        requirement, sub, redefinition = package.children
        self.assertEqual(requirement.short_name, "G.4")
        self.assertEqual(sub.specializes, "Super")
        self.assertEqual(redefinition.redefines, "status")
        self.assertEqual(redefinition.value, "StageStatus::Done")

    def test_multiplicity_and_ordering_leave_the_type_clean(self):
        package = only(parse("package P { attribute regions : MemRegion[1..*] ordered; }")[0])
        element = package.children[0]
        self.assertEqual(element.typed_by, "MemRegion")
        self.assertEqual(element.multiplicity, "1..*")

    def test_a_value_may_hold_commas_parentheses_and_colons(self):
        package = only(
            parse(
                'package P { attribute user = AddressRange(base = "0x00", limit = "0x7F", '
                'purpose = "user"); }'
            )[0]
        )
        self.assertIn('base = "0x00"', package.children[0].value)
        self.assertIn('purpose = "user"', package.children[0].value)

    def test_a_string_value_is_prose_not_notation(self):
        # A requirement's statement names `#PF`, `<'H.1'>`, `[1..*]`, `;`,
        # `{` and `=` as readily as any sentence, and wraps across lines.
        text = (
            "package P { requirement <'H.T.1'> r : ItemHighLevel {\n"
            "    attribute :>> statement = \"A #PF from ring 3 shall be\n"
            "        a signal; never <'X'> [1..*] {halt} a = b.\";\n"
            "    attribute :>> parent = (\"O.ISOLATE\", \"ASR-1\");\n"
            "} part def After; }"
        )
        root, _, unparsed = parse(text)
        requirement, after = only(root).children
        self.assertEqual(unparsed, [])
        self.assertEqual(requirement.short_name, "H.T.1")
        self.assertEqual(requirement.keywords, [])
        self.assertEqual(
            requirement.attribute_value("statement"),
            '"A #PF from ring 3 shall be a signal; never <\'X\'> [1..*] {halt} a = b."',
        )
        self.assertEqual(requirement.attribute_value("parent"), '("O.ISOLATE", "ASR-1")')
        # The lines a string spans are still counted.
        self.assertEqual(after.line, 5)

    def test_modifiers_are_collected(self):
        package = only(parse("package P { abstract variation part def Layer :> Facade; }")[0])
        self.assertEqual(package.children[0].modifiers, ["abstract", "variation"])

    def test_enum_literals_become_children(self):
        package = only(parse("package P { enum def Isa { X86_64; AArch64; Armv7a; } }")[0])
        literals = package.children[0].children
        self.assertEqual([literal.name for literal in literals], ["X86_64", "AArch64", "Armv7a"])
        self.assertTrue(all(literal.kind == "enum-literal" for literal in literals))


class Comments(unittest.TestCase):
    def test_a_doc_comment_describes_the_body_it_opens(self):
        package = only(parse("package P { part def A { doc /* About A. */ part b : B; } }")[0])
        element = package.children[0]
        self.assertEqual(element.doc, "About A.")
        # Emphatically not the declaration that follows it.
        self.assertEqual(element.children[0].doc, "")

    def test_a_doc_comment_closing_a_body_is_not_lost(self):
        # There is no `;` after it, so the brace has to claim it.
        package = only(parse("package P { part def A { doc /* Trailing. */ } }")[0])
        self.assertEqual(package.children[0].doc, "Trailing.")

    def test_hard_wraps_are_undone_and_blank_lines_kept(self):
        text = """package P {
    part def A {
        doc /* One sentence
             * wrapped across lines.
             *
             * A second paragraph. */
    }
}"""
        element = only(parse(text)[0]).children[0]
        self.assertEqual(element.doc, "One sentence wrapped across lines.\n\nA second paragraph.")

    def test_a_brace_inside_prose_does_not_open_a_body(self):
        package = only(parse("package P { part def A { doc /* Uses { and } here. */ } }")[0])
        self.assertEqual(len(package.children), 1)
        self.assertIn("{", package.children[0].doc)

    def test_line_comments_are_dropped_and_line_numbers_survive(self):
        text = "package P {\n// a banner\n// another\npart def A;\n}"
        element = only(parse(text)[0]).children[0]
        self.assertEqual(element.name, "A")
        self.assertEqual(element.line, 4)


class Relations(unittest.TestCase):
    def test_satisfy_allocate_and_dependency(self):
        _, relations, _ = parse(
            "package P {"
            " satisfy stage1 by ferrix.loader;"
            " allocate stage6 to ferrix.kernel.vm;"
            " dependency from stage2 to stage1, stage0;"
            " }"
        )
        by_kind = {relation.kind: relation for relation in relations}
        self.assertEqual(by_kind["satisfy"].source, "stage1")
        self.assertEqual(by_kind["satisfy"].targets, ["ferrix.loader"])
        self.assertEqual(by_kind["allocate"].targets, ["ferrix.kernel.vm"])
        self.assertEqual(by_kind["dependency"].targets, ["stage1", "stage0"])

    def test_verify_records_the_enclosing_verification(self):
        _, relations, _ = parse(
            "package P { verification bootX86 : BootTest { objective { verify stage1; } } }"
        )
        verify = [relation for relation in relations if relation.kind == "verify"][0]
        self.assertEqual(verify.targets, ["stage1"])
        self.assertTrue(verify.origin.endswith("bootX86"))

    def test_a_visibility_word_does_not_hide_an_import(self):
        _, relations, _ = parse("package P { private import ScalarValues::*; }")
        self.assertEqual([relation.kind for relation in relations], ["import"])


class Flows(unittest.TestCase):
    def test_first_and_then_give_the_step_order(self):
        package = only(
            parse(
                "package P { action def Boot {"
                " first start;"
                " then action validate;"
                " then action install { doc /* Before anything can fault. */ }"
                " then done;"
                " } }"
            )[0]
        )
        action = package.children[0]
        self.assertEqual(action.flow, ["start", "validate", "install", "done"])
        self.assertEqual(action.child("install").doc, "Before anything can fault.")

    def test_a_guard_clause_does_not_become_an_element(self):
        package = only(
            parse(
                "package P { action def D {"
                " first start;"
                " then decide;"
                " if trap.kind == TrapKind::Breakpoint then breakpoint;"
                " } }"
            )[0]
        )
        kinds = {child.kind for child in package.children[0].children}
        self.assertIn("clause", kinds)
        self.assertNotIn("", kinds)


class UnknownNotation(unittest.TestCase):
    def test_an_unrecognised_declaration_is_reported_not_dropped(self):
        _, _, unparsed = parse("package P { ?? nonsense ?? ; }")
        self.assertEqual(len(unparsed), 1)
        self.assertIn("nonsense", unparsed[0].raw)


class MarkdownEscaping(unittest.TestCase):
    def test_a_pipe_cannot_eat_a_table_column(self):
        table = document.Table(head=["A", "B"], rows=[["x|y", "z"]])
        row = "\n".join(render_markdown._table(table)).splitlines()[2]
        self.assertIn(r"x\|y", row)
        # The escaped pipe must not count as a column delimiter, so the row
        # still has exactly the three that bound two cells.
        delimiters = len(re.findall(r"(?<!\\)\|", row))
        self.assertEqual(delimiters, 3, row)

    def test_intraword_underscores_are_left_alone(self):
        self.assertEqual(render_markdown.escape("set_tid_address"), "set_tid_address")
        self.assertEqual(render_markdown.escape("_emphasis"), "\\_emphasis")

    def test_ordinary_punctuation_is_not_escaped(self):
        self.assertEqual(render_markdown.escape("docs/ROADMAP.md (§9)"), "docs/ROADMAP.md (§9)")

    def test_a_leading_hash_is_defused(self):
        self.assertEqual(render_markdown.escape("# not a heading"), "\\# not a heading")

    def test_a_single_tilde_is_prose_and_a_double_is_not(self):
        self.assertEqual(render_markdown.escape("~2 GiB"), "~2 GiB")
        self.assertIn("\\~", render_markdown.escape("~~struck~~"))

    def test_a_code_span_holding_a_backtick_gets_a_longer_fence(self):
        rendered = render_markdown.inline([document.c("a ` b")])
        self.assertTrue(rendered.startswith("``"))
        self.assertIn("a ` b", rendered)


class HtmlEscaping(unittest.TestCase):
    def test_angle_brackets_and_ampersands_are_escaped(self):
        rendered = render_html.inline([document.t("a < b & c > d")])
        self.assertNotIn("<b", rendered)
        self.assertIn("&lt;", rendered)
        self.assertIn("&amp;", rendered)

    def test_a_maturity_keyword_is_rendered_as_a_badge(self):
        self.assertIn('class="mat mat-implemented"', render_html.inline([document.c("#implemented")]))
        self.assertIn("<code>", render_html.inline([document.c("src/lib/kernel/sched")]))


class Prose(unittest.TestCase):
    def test_backtick_spans_in_model_prose_become_code_runs(self):
        parts = sections.prose("Reached by `src/lib/platform/fdt` at stage 1.")
        self.assertEqual(parts[1], document.c("src/lib/platform/fdt"))

    def test_an_unbalanced_backtick_stays_literal(self):
        parts = sections.prose("a ` b")
        self.assertEqual(parts, [document.t("a ` b")])

    def test_first_sentence_does_not_break_on_a_section_sign(self):
        parts = sections.first_sentence("docs/ARCHITECTURE.md §4. The physical allocator runs.")
        self.assertIn("physical allocator", document.plain(parts))


class Humanise(unittest.TestCase):
    def test_sentence_case_not_title_case(self):
        self.assertEqual(humanise("kernelThreads"), "Kernel threads")

    def test_a_stage_prefix_becomes_a_number(self):
        self.assertEqual(humanise("stage3TrapsInterruptsTime"), "Stage 3 traps interrupts time")

    def test_known_acronyms_and_names_keep_their_spelling(self):
        self.assertEqual(humanise("stage7LinuxAbi"), "Stage 7 Linux ABI")
        self.assertEqual(humanise("iommuIsNotOptional"), "IOMMU is not optional")


class Successions(unittest.TestCase):
    """What an action's body says about order, which is what a flow draws."""

    def test_a_straight_chain_is_recorded_in_order(self):
        package = only(
            parse(
                """
                package P {
                    action def A {
                        action one;
                        action two;
                        first start;
                        then one;
                        then two;
                        then done;
                    }
                }
                """
            )[0]
        )
        action = package.children[0]
        self.assertEqual(
            action.successions,
            [("start", "one", ""), ("one", "two", ""), ("two", "done", "")],
        )

    def test_a_decide_records_both_branches_with_their_guards(self):
        package = only(
            parse(
                """
                package P {
                    action def A {
                        first start;
                        then decide;
                            if fault.write then copy;
                            else zero;
                    }
                }
                """
            )[0]
        )
        action = package.children[0]
        self.assertIn(("decide", "copy", "fault.write"), action.successions)
        self.assertIn(("decide", "zero", "else"), action.successions)

    def test_a_rejoining_chain_is_not_read_as_one_line(self):
        """`first x then done` starts again; the flat reading loses that."""
        package = only(
            parse(
                """
                package P {
                    action def A {
                        first start;
                        then decide;
                            if g then left;
                            else right;
                        first left then done;
                        first right then done;
                    }
                }
                """
            )[0]
        )
        action = package.children[0]
        self.assertIn(("left", "done", ""), action.successions)
        self.assertIn(("right", "done", ""), action.successions)

    def test_a_transition_carries_both_ends_and_its_trigger(self):
        package = only(
            parse(
                """
                package P {
                    state def S {
                        state running;
                        state draining;
                        transition t first running accept request : Req then draining;
                    }
                }
                """
            )[0]
        )
        transition = [c for c in package.children[0].children if c.kind == "transition"][0]
        self.assertEqual(transition.flow, ["running", "draining"])
        self.assertEqual(transition.value, "request : Req")


class Resolution(unittest.TestCase):
    def test_a_replaced_list_of_roots_is_what_names_are_found_in(self):
        def roots_of(text):
            root, _, _ = parse_text("test.sysml", text)
            for element in root.children:
                element.parent = None
            return root.children

        model = load([], ROOT)
        model.roots = roots_of("package Before { requirement one; }")
        self.assertIsNotNone(model.find("Before::one"))
        self.assertIsNotNone(model.resolve("one"))
        # A lookup has made the table; replacing the roots must not leave it.
        model.roots = roots_of("package After { requirement two; }")
        self.assertIsNone(model.find("Before::one"))
        self.assertIsNone(model.resolve("one"))
        self.assertEqual(model.find("After::two").qualified_name, "After::two")
        self.assertEqual(model.resolve("two").qualified_name, "After::two")

    def test_a_reference_resolves_inside_the_package_that_wrote_it(self):
        model = load([], ROOT)
        root, _, _ = parse_text(
            "test.sysml",
            """
            package Early { action def Run { action stageFive; } }
            package Late { requirement stageFive; }
            """,
        )
        model.roots = root.children
        for element in model.roots:
            element.parent = None
        self.assertEqual(
            model.resolve("stageFive", scope="Late").qualified_name, "Late::stageFive"
        )
        # Without the scope the earlier declaration wins, which is the whole
        # reason the scope is passed.
        self.assertEqual(
            model.resolve("stageFive").qualified_name, "Early::Run::stageFive"
        )

    def test_the_nearest_enclosing_scope_wins(self):
        """A crate and a part of the kernel may share a name; the statement
        that names one is written inside it."""
        model = load([], ROOT)
        root, _, _ = parse_text(
            "test.sysml",
            """
            package S {
                part def Kernel { part acpi : AcpiAccess; }
                part def Workspace { part acpi : LibraryCrate; }
            }
            """,
        )
        model.roots = root.children
        for element in model.roots:
            element.parent = None
        self.assertEqual(
            model.resolve("acpi", scope="S::Workspace").qualified_name,
            "S::Workspace::acpi",
        )


class Figures(unittest.TestCase):
    def _figure(self):
        graph = figure.Figure(name="f", title="F", kind="block")
        graph.add(figure.Node(id="a", label="a"))
        graph.add(figure.Node(id="b", label="b"))
        return graph

    def test_an_edge_with_an_end_that_is_not_drawn_is_dropped(self):
        graph = self._figure()
        graph.link("a", "missing")
        self.assertEqual(graph.edges, [])

    def test_the_same_pair_joined_twice_keeps_both_labels(self):
        graph = self._figure()
        graph.link("a", "b", "connect", "posix → linuxAbi")
        graph.link("a", "b", "connect", "native → nativeAbi")
        self.assertEqual(len(graph.edges), 1)
        self.assertEqual(graph.edges[0].label, "posix → linuxAbi · native → nativeAbi")

    def test_a_figure_with_no_edge_is_empty(self):
        self.assertTrue(self._figure().is_empty)


class Layout(unittest.TestCase):
    def _chain(self, count: int, direction: str = "TB") -> figure.Figure:
        graph = figure.Figure(name="c", title="C", kind="flow", direction=direction)
        for index in range(count):
            graph.add(figure.Node(id=str(index), label=f"step{index}"))
        for index in range(count - 1):
            graph.link(str(index), str(index + 1))
        return graph

    def test_the_same_figure_lays_out_the_same_way_twice(self):
        graph = self._chain(8)
        first = render_svg.render(graph)
        second = render_svg.render(graph)
        self.assertEqual(first, second)

    def test_no_two_boxes_overlap(self):
        graph = figure.Figure(name="f", title="F", kind="block", direction="LR")
        graph.add(figure.Node(id="root", label="root"))
        for index in range(24):
            graph.add(figure.Node(id=str(index), label=f"member{index}"))
            graph.link("root", str(index), "composition")
        placed = layout.compute(graph)
        boxes = placed.boxes
        for left in range(len(boxes)):
            for right in range(left + 1, len(boxes)):
                one, two = boxes[left], boxes[right]
                overlap = (
                    one.x < two.x + two.width
                    and two.x < one.x + one.width
                    and one.y < two.y + two.height
                    and two.y < one.y + one.height
                )
                self.assertFalse(overlap, f"{one.node.id} overlaps {two.node.id}")

    def test_a_wide_fan_is_dealt_into_more_than_one_line(self):
        graph = figure.Figure(name="f", title="F", kind="block", direction="LR")
        graph.add(figure.Node(id="root", label="root"))
        for index in range(30):
            graph.add(figure.Node(id=str(index), label=f"member{index}"))
            graph.link("root", str(index), "composition")
        placed = layout.compute(graph)
        columns = {round(box.x) for box in placed.boxes}
        self.assertGreater(len(columns), 2)

    def test_everything_drawn_is_inside_the_canvas(self):
        placed = layout.compute(self._chain(6, direction="LR"))
        for box in placed.boxes:
            self.assertGreaterEqual(box.x, 0)
            self.assertGreaterEqual(box.y, 0)
            self.assertLessEqual(box.x + box.width, placed.width)
            self.assertLessEqual(box.y + box.height, placed.height)

    def test_flipping_puts_the_last_layer_first(self):
        graph = self._chain(3)
        upright = layout.compute(graph)
        graph.flip = True
        flipped = layout.compute(graph)
        self.assertLess(upright.box("0").y, upright.box("2").y)
        self.assertGreater(flipped.box("0").y, flipped.box("2").y)

    def test_a_cycle_does_not_hang_and_keeps_its_direction(self):
        graph = self._chain(3)
        graph.link("2", "0")
        placed = layout.compute(graph)
        back = [route for route in placed.routes if route.edge.source == "2"][0]
        self.assertEqual(back.points[0][1] > back.points[-1][1], True)


class Svg(unittest.TestCase):
    def _figure(self):
        graph = figure.Figure(name="fig", title="A < B & C", kind="flow")
        graph.add(figure.Node(id="a", label='a < b & "c"', doc="Prose with <angles>."))
        graph.add(figure.Node(id="b", label="b", maturity="implemented"))
        graph.link("a", "b", "flow", "guard < 2")
        return graph

    def test_the_output_is_well_formed_xml(self):
        xml.dom.minidom.parseString(render_svg.render(self._figure(), standalone=True))

    def test_markup_in_a_label_is_escaped(self):
        rendered = render_svg.render(self._figure())
        self.assertNotIn("<b &", rendered)
        self.assertIn("&lt;", rendered)
        self.assertIn("&amp;", rendered)

    def test_marker_ids_carry_the_figure_name(self):
        rendered = render_svg.render(self._figure())
        self.assertIn('id="dg-fig-arrow"', rendered)
        self.assertIn("url(#dg-fig-arrow)", rendered)

    def test_a_standalone_file_carries_its_own_palette(self):
        rendered = render_svg.render(self._figure(), standalone=True)
        self.assertIn("xmlns=", rendered)
        self.assertIn("prefers-color-scheme", rendered)
        self.assertNotIn("prefers-color-scheme", render_svg.render(self._figure()))


class Mermaid(unittest.TestCase):
    def test_characters_that_would_end_a_label_early_are_entities(self):
        graph = figure.Figure(name="f", title="F", kind="block")
        graph.add(figure.Node(id="a", label='#[expect] "x" [0..*]'))
        graph.add(figure.Node(id="b", label="b"))
        graph.link("a", "b")
        rendered = render_mermaid.render(graph)
        self.assertNotIn('"x"', rendered)
        self.assertIn("#35;", rendered)
        self.assertIn("#quot;", rendered)
        self.assertIn("#91;0..*#93;", rendered)

    def test_a_state_figure_uses_the_state_notation(self):
        graph = figure.Figure(name="f", title="F", kind="state")
        graph.add(figure.Node(id="a", label="running", kind="state"))
        graph.add(figure.Node(id="b", label="draining", kind="state"))
        graph.link("a", "b", "transition", "request")
        rendered = render_mermaid.render(graph)
        self.assertTrue(rendered.startswith("stateDiagram-v2"))
        self.assertIn(" : request", rendered)

    def test_every_node_gets_an_identifier_mermaid_accepts(self):
        """A qualified name is not an identifier; the folded form has to be."""
        graph = figure.Figure(name="f", title="F", kind="block")
        graph.add(figure.Node(id="Ferrix::Kernel.mm", label="mm"))
        graph.add(figure.Node(id="Ferrix::Kernel.vm", label="vm"))
        identifiers = {render_mermaid.node_id(graph, node) for node in graph.nodes}
        self.assertEqual(len(identifiers), len(graph.nodes))
        for identifier in identifiers:
            self.assertRegex(identifier, r"^[A-Za-z][A-Za-z0-9_]*$")


class TheRealModel(unittest.TestCase):
    """The guard that matters: the reader keeps up with docs/sysml/."""

    @classmethod
    def setUpClass(cls):
        paths = sorted((ROOT / "docs" / "sysml").glob("*.sysml"))
        if not paths:
            raise unittest.SkipTest("no model files")
        cls.model = load(paths, ROOT)

    def test_every_declaration_is_understood(self):
        listed = "\n".join(
            f"  {element.source}:{element.line}: {element.raw}" for element in self.model.unparsed
        )
        self.assertEqual(self.model.unparsed, [], f"unrecognised declarations:\n{listed}")

    def test_the_packages_all_loaded(self):
        names = {root.name for root in self.model.roots}
        self.assertIn("FerrixStructure", names)
        self.assertIn("FerrixRoadmap", names)
        self.assertEqual(len(self.model.roots), len(self.model.files))

    def test_every_maturity_keyword_the_lifecycle_defines_is_recognised(self):
        counts = self.model.maturity_counts()
        self.assertTrue(sum(counts.values()) > 0)
        self.assertTrue(counts["implemented"] > 0)

    def test_generation_is_reproducible(self):
        first = sections.build(self.model, banner="b")
        second = sections.build(self.model, banner="b")
        self.assertEqual(render_markdown.render(first), render_markdown.render(second))
        self.assertEqual(render_html.render(first), render_html.render(second))
        self.assertEqual(emit_json.render(self.model), emit_json.render(self.model))

    def test_the_rendered_document_holds_no_placeholder(self):
        rendered = render_markdown.render(sections.build(self.model, banner="b"))
        self.assertNotIn("\x00", rendered)

    def test_the_document_draws_figures(self):
        placed = sections.build(self.model, banner="b").meta.get("figures") or []
        self.assertTrue(placed, "the model holds flows, states and relations to draw")
        names = [block.figure.name for block in placed]
        self.assertEqual(len(names), len(set(names)), "two figures share a file name")
        for block in placed:
            with self.subTest(block.figure.name):
                self.assertFalse(block.figure.is_empty)
                self.assertTrue(all(node.label for node in block.figure.nodes))
                self.assertEqual(block.file, f"diagrams/{block.figure.name}.svg")

    def test_every_figure_renders_as_well_formed_svg(self):
        for block in sections.build(self.model, banner="b").meta.get("figures") or []:
            with self.subTest(block.figure.name):
                xml.dom.minidom.parseString(
                    render_svg.render(block.figure, standalone=True)
                )

    def test_every_figure_is_drawn_from_elements_the_model_declares(self):
        """A node invented by the builder would be a picture of nothing."""
        for block in sections.build(self.model, banner="b").meta.get("figures") or []:
            for node in block.figure.nodes:
                if node.kind in ("terminal", "choice"):
                    continue  # `start`, `done`, `decide`: the notation's words
                with self.subTest(f"{block.figure.name}:{node.id}"):
                    qualified = node.id.split("::")
                    self.assertTrue(
                        self.model.find(node.id) is not None
                        or self.model.resolve(qualified[-1]) is not None,
                        f"{node.id} is not in the model",
                    )

    def test_the_flow_of_the_loader_matches_the_figure_drawn_of_it(self):
        action = self.model.find("FerrixBoot::LoaderSequence")
        drawn = diagrams.flow(self.model, "FerrixBoot::LoaderSequence")
        self.assertEqual(len(drawn.edges), len(action.successions))

    def test_every_committed_output_is_current(self):
        generated = ROOT / "docs" / "generated"
        if not (generated / "ARCHITECTURE.md").exists():
            self.skipTest("nothing generated yet")
        # The generator itself, not a copy of what it does: a test that
        # rebuilt the outputs its own way would pass while the command that
        # writes them was broken.
        outputs, _ = _generator().build(ROOT, lenient=False)
        for name, text in outputs.items():
            with self.subTest(name):
                path = generated / name
                self.assertTrue(path.exists(), f"{name} is missing; run `cargo xtask model-doc`")
                self.assertEqual(
                    path.read_text(encoding="utf-8"),
                    text,
                    f"{name} is stale; run `cargo xtask model-doc`",
                )

    def test_no_diagram_is_committed_that_nothing_draws(self):
        directory = ROOT / "docs" / "generated" / "diagrams"
        if not directory.is_dir():
            self.skipTest("nothing generated yet")
        outputs, _ = _generator().build(ROOT, lenient=False)
        expected = {name.rsplit("/", 1)[-1] for name in outputs if name.startswith("diagrams/")}
        for path in sorted(directory.glob("*.svg")):
            self.assertIn(path.name, expected, f"{path.name} is left over; regenerate")


def _generator():
    """`tools/common/gen/gen-arch-doc.py`, imported despite the hyphen in its name."""
    path = ROOT / "tools" / "common" / "gen" / "gen-arch-doc.py"
    spec = importlib.util.spec_from_file_location("gen_arch_doc", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


if __name__ == "__main__":
    unittest.main(verbosity=2)
