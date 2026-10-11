---
name: pdf-report
description: Produce a PDF document (report, summary, letter, invoice) and deliver it to the user. Use whenever the user asks for a PDF or for something "as a document" or "to print".
---

# PDF report

You write Markdown. A tool turns it into the PDF.

1. Create the document as a Markdown file in the working directory with the Write tool (not with a shell command). Give it a short descriptive name such as `sales-report.md`. Use ordinary Markdown only:

   ```markdown
   # Title of the document

   An opening paragraph.

   ## A section

   Text with **bold** and *italic*, a [link](https://example.com), and a list:

   - first point
   - second point

   | Column A | Column B |
   |----------|----------|
   | value    | value    |
   ```

   - Begin with one `# Title` line. Use `##` for sections and `###` for subsections.
   - Write each paragraph on a single line. A new line in the file is a new line in the PDF, which is what you want for an address or a list of details. Leave an empty line between paragraphs.
   - Every table needs its header row and the `|---|` line under it. Put totals in the same table as the rows they add up.
   - Any language works, including Chinese, Japanese and Korean.
   - For a line break inside a table cell, write `<br>`.
   - To include an image, save it next to the Markdown file and write `![caption](image.png)`. A large image is made smaller to fit the page.
   - A table can have any number of rows. A long one continues on the next page by itself.
   - Do not write HTML, LaTeX or page-layout commands. Page size, fonts, page numbers and page breaks are set for you.

2. Convert it. Run this command on its own, not joined to other commands:

   ```bash
   md2pdf sales-report.md
   ```

   It creates `/work/out/sales-report.pdf` and prints the path and the number of pages, so you do not need to inspect the file. If it prints an error, fix the Markdown file and run the same command again. If the user asked for a page limit and there are too many pages, shorten the Markdown and run it again.

3. Reply with one or two sentences saying what the document contains. The PDF is sent to the user automatically. Do not paste the document into the chat.
