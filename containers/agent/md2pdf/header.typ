// Look of the PDFs md2pdf produces. Pandoc pastes this in ahead of the document.

// The first font covers Latin text; the next ones are used for the characters it lacks.
// Put the Noto CJK variant for your users' language first among them: SC, TC, JP or KR.
#set text(font: (
  "Libertinus Serif",
  "Noto Serif CJK SC",
  "Noto Sans CJK SC",
  "Noto Color Emoji",
))
#show raw: set text(font: ("DejaVu Sans Mono", "Noto Sans Mono CJK SC"))

#show heading: set block(above: 1.4em, below: 0.8em)
#show raw.where(block: true): block.with(fill: luma(245), inset: 8pt, radius: 3pt, width: 100%)
#show quote.where(block: true): block.with(stroke: (left: 2pt + luma(190)), inset: (left: 10pt, y: 3pt))
#set table(stroke: 0.5pt + luma(170))

// Pandoc puts every table in a figure, and Typst keeps a figure on one page. The rows of
// a long table that did not fit were printed on top of each other at the foot of it.
// Allowed to break, a table runs on over as many pages as it needs, and its header row
// is repeated at the top of each.
#show figure.where(kind: table): set block(breakable: true)
// A short table is still kept whole, so that one starting near the foot of a page does
// not leave a row or two behind there.
#show figure.where(kind: table): it => layout(available => {
  if measure(width: available.width, it).height > page.height / 3 { return it }
  block(breakable: false, it)
})

// A picture is printed at its own size unless that is wider than the text or taller than
// a third of the page; then it is scaled down until it is neither. Left to Typst it only
// has to fit on the page, so a phone screenshot or a portrait photo took a whole one.
#show image: it => layout(available => {
  let own = measure(it)
  let scale = calc.min(1, available.width / own.width, page.height / 3 / own.height)
  box(width: own.width * scale, height: own.height * scale, it)
})
