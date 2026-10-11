---
name: mcp-files
description: Get a file that a tool saved on the MCP server, for example a photo or document from `download_media`, so that you can read it or send it to the user. Use whenever a tool result gives you the path of a file instead of the file itself.
---

# Files from the MCP server

The tools of the `domain` server run on another machine. When one of them saves a file, the file is on that machine, not here. You cannot open the path it reports. One command copies the file here.

1. Call the tool that saves the file, for example `download_media`. Its result gives the path of the file on that machine, such as `/home/mcp/files/-1001234567890_45_menu.pdf`.

2. Run one of these two commands on its own, not joined to other commands. Put the path in single quotes, exactly as the tool gave it, with nothing added or removed.

   To read the file or use it yourself:

   ```bash
   mcp-file get '/home/mcp/files/-1001234567890_45_menu.pdf'
   ```

   It saves the file in `/work/in/` and prints where, for example `saved /work/in/-1001234567890_45_menu.pdf`. Open that path with the Read tool. This works for images and PDFs too.

   To give the file to the user:

   ```bash
   mcp-file deliver '/home/mcp/files/-1001234567890_45_menu.pdf'
   ```

   It saves the file in `/work/out/`. The user receives it automatically when you finish your reply.

3. If the command prints an error, read what it says.

   - `has no file at`: the server no longer keeps the file, or the path is not the one the tool gave. Check the path. If it is right, call the tool from step 1 again, then run the same command again.
   - Anything else: tell the user what the error says. Do not try `curl` or another command instead.
