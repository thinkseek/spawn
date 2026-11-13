Third-party notices: Spawn web dashboard
=========================================

persona (https://github.com/nwverse/persona)
-------------------------------------------
Spawn's theme layer (src/styles/spawn.css) follows persona's token roles
(page, canvas, surface, inset, hover, line, line-strong, ink, ink-2, ink-3,
pale status tints), its shadow ladder (hairline, card, raised, overlay: a
1 px ring in the line colour plus soft shadows), its status roles for agents
(needs you, working, waiting, ok, failed, idle), and its fixed sidebar row
heights (32 px, 44 px on touch). persona takes those tokens from Beautiful
UI (below). `selected` and the colour values are Spawn's own (oklch, low
saturation). Used under the MIT License:

MIT License

Copyright (c) 2026 nwverse

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

Beautiful UI (https://www.beautifului.dev/)
-------------------------------------------
persona's web tokens (its styles.css: "Design tokens, keyframes and component
grammar from Beautiful UI") are Beautiful UI's, so the roles above come from
there through persona. Used under the MIT License:

MIT License

Copyright (c) 2026 Shane Levine

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

Multica
-------
src/styles/tokens.css and src/styles/base.css: see NOTICE and
LICENSE-MULTICA in this directory.

affectUI (https://www.affectui.com)
-----------------------------------
src/components/ui/*.tsx (20 files): affectUI's free components, each taken
unchanged from its registry item at https://www.affectui.com/r/<name>.json
on 2026-11-13 (the item's one file, byte for byte). The sha256 of each file
as taken is in src/components/ui/affectui.sha256. affectUI states these
items are "free and open source (MIT)" (https://www.affectui.com/llms.txt,
read 2026-11-13). No item of affectUI Pro is used or copied.

The registry files carry no licence header and no copyright line, and none
is published at a plain address of the site; the holder's line is added
here when the owner has it from affectUI's licence page. Used under the
MIT License:

MIT License

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
