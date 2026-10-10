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
