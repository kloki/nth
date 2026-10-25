---
name: curios-design
description: Design guidance for Curios - what a curio's HTML may contain, how to give it a deliberate visual treatment and how to draw diagrams in it. Load before writing or restyling a curio, alongside the curios skill.
---

Work the way the design lead at a small, versatile studio would: give each document a visual identity at the level of treatment its brief calls for. Make deliberate choices about palette, typography and layout that are specific to this subject, and avoid templated designs.

## Page contract

Publishing validates every curio and the viewer's frame enforces the same limits, so a curio that breaks one is refused rather than shown broken. Design within them from the start:

- **One complete HTML file**, at most 4 MB. Nothing wraps it: write the `<!doctype html>`, `<html lang>`, `<meta charset>`, the viewport meta and a `<title>` yourself, with one `<style>` block in the head.
- **No scripts or interactive elements.** No `script`, `iframe`, `object`, `embed`, `link`, `form`, `input`, `button`, `textarea`, `select`, `audio`, `video`, no `on*` attributes, `contenteditable`, `autofocus` or `http-equiv` meta. For disclosure use `<details>` and `<summary>`; hover and focus states are CSS only.
- **No web fonts.** `@font-face` and `@import` are refused and the frame loads no fonts. Type comes from system font stacks (below).
- **CSS has no comments and no escapes.** `/*` and `\` are refused. Write characters literally (`content: "—"`, `content: "“"`), never `\2014`. `url()` may only name an element of the page, `url(#id)`.
- **No network and no navigation.** `href` may only be a `#fragment` in the same page; there are no links out, so name a source in text instead. Images are only embedded `data:` PNG, JPEG, GIF or WebP, at most 8192 px on a side; prefer inline SVG for anything drawn.
- **SVG is static.** No `foreignObject`, `animate`, `animateMotion`, `animateTransform` or `set`; gradients, patterns, markers and `<use>` reference ids in the same page.
- **Only elements with an `id` can be commented on.** IDs are unique, nonempty and at most 160 bytes. A comment anchor is labelled with the element's `aria-label`, else its text.
- **Themes come only from `prefers-color-scheme`.** The viewer sets no theme attribute on the page.

If publishing refuses the document, its message names the rule: fix that and publish again.

## Read the request first

Decide the treatment; designing is a given. A memo gets the same craft as a launch page; only the treatment differs.

Most curios call for a utilitarian treatment: a plan, a review document, a report, a comparison. Make it polished, with real typographic hierarchy, considered spacing and a proper palette, but do not over-design it. Most documents need no hero. Quiet Folio, the starter `curio_read` returns, is a good utilitarian base: start from it and give it this subject's details.

Some briefs call for an editorial treatment: a pitch, a showcase, a page the user will keep or pass around. Then follow the editorial section below.

When unsure, a well-composed document is always acceptable; an over-designed visual identity sometimes is not.

## Fundamentals

**Respect what already exists.** Look for a design system first: the brief, `AGENTS.md` or `CLAUDE.md`, a tokens or theme file in the project, the curio's own earlier revisions. When one exists, apply it; everything below fills gaps. Precedence is the user's words, then the project's system, then your own choices.

**Ground it in the subject.** Name one concrete subject, its reader and the document's single job. Distinctive choices come from the subject's own world: its materials, instruments and vernacular. Include at least one detail only this subject would have (its real units and scales, its document conventions, its terms of art) as content rather than ornament. Use real content throughout; label examples and assumptions as such and never invent metrics.

**Pair typefaces from system stacks.** Without web fonts, character comes from choosing the stack and from weight, size, width and tracking. Some stacks that hold up across platforms:

- Book serif: `"Iowan Old Style", "Palatino Linotype", Palatino, "Book Antiqua", Georgia, serif`
- Transitional serif: `Charter, "Bitstream Charter", "Sitka Text", Cambria, Georgia, serif`
- Neutral sans: `system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif`
- Humanist sans: `Seravek, "Gill Sans Nova", Ubuntu, Calibri, "DejaVu Sans", source-sans-pro, sans-serif`
- Geometric sans: `Avenir, Montserrat, Corbel, "URW Gothic", source-sans-pro, sans-serif`
- Monospace: `ui-monospace, "Cascadia Code", "SF Mono", Menlo, Consolas, "DejaVu Sans Mono", monospace`

Pair a display role with a body role (and a mono role for data or code if needed). Keep running text near 65 characters wide, set a type scale and keep to it, give headings `text-wrap: balance`, body text comfortable line height, and uppercase labels a little letter-spacing.

**Choose neutrals deliberately.** A pure mid-grey looks unconsidered; a grey with a slight hue bias toward the accent looks intentional. Pure white and near-black are fine grounds when they suit the subject, as long as you chose them.

**Define colour as tokens.** Put every colour on `:root` as a custom property, and style components only through them. To follow a dark system theme, redefine only the tokens in `@media (prefers-color-scheme: dark) { :root { ...; color-scheme: dark } }`, giving the dark palette the same care rather than inverting it, and keeping the accent legible on both grounds. A document that commits to one look may stay single-theme by choice: then set `color-scheme` on `:root` to match. Either way `body` sets an explicit `background` from a token.

**Use layout for spacing.** Lay out sibling groups with flex or grid and `gap` instead of per-element margins. Keep a side gutter of at least 16 px at every width. The document must work at phone width (about 400 px) and the body must never scroll sideways: let rows wrap or stack, give flex and grid children that hold text, code or a table `min-width: 0`, give images `max-width: 100%`, and let only wide tables, diagrams and code exceed the width, each in its own `overflow-x: auto` container. Use `font-variant-numeric: tabular-nums` wherever digits line up.

**Make repeated elements consistent.** Cards in a row, label/value pairs down a list, badges on sibling items: same edges, baselines and inner padding on each, recurring parts in the same place. Let content set heights, and pick a column count the items fill. Text that can outgrow its track wraps or scrolls in its own container; clipped text is a bug.

**Use card styling selectively.** Border, fill, radius and shadow each mark an element as a separate object. Apply them by role to set off the one element that needs it; the same radius and shadow on every block flattens the hierarchy. Open with big-number tiles only when those figures are the point of the document.

**Draw charts to scale.** Place marks, ticks and labels with one scale, and make every label name a value the chart reaches. Colour chart text from the tokens so it reads in both themes. Keep marks, labels and edges clear of one another and inside the drawing; leave room in the `viewBox` for the outermost labels and give every shape an explicit fill.

**Make the page complete at rest.** There is no script to reveal anything, so everything meant to be read is visible on load. Any CSS animation starts from a visible state, never `opacity: 0`, and is turned off under `prefers-reduced-motion`. Size a hero to its content, not to the viewport.

**Avoid AI-generated design.** It currently clusters around a few looks: warm cream with a serif display and terracotta accent; near-black with a lone acid-green or vermilion pop; broadsheet hairline rules with dense columns; a purple-to-blue gradient hero on white; emoji as section markers; everything centred; large radius everywhere; an accent bar down the side of rounded cards. When the user names a direction, follow it exactly, even toward one of these. When nothing is named, don't spend that freedom on one of these defaults.

**Build cleanly.** Watch for overlapping elements, cascade collisions and specificity that cancels your spacing. Close every non-void element, double-quote attributes, give `<summary>` and `#` links a visible focus state. Give tables a `<caption>` and `<th>` header cells.

**Write the copy as design material.** Write from the reader's side: name things by what people recognise rather than how the system is built. Use active voice and short, direct sentences. Avoid mannered devices: asides set off by dashes, "not X, but Y" framing, colon-then-reveal sentences, scare quotes and stock phrases.

**Name the document like a product.** The `<title>` is what the viewer and the curio list show. Give it a real name: a short noun phrase, typically two to four words, specific to the subject, or the one question the document answers. Use the user's own name for the thing when they have one. No explanation after a dash or colon, and never a bare category label.

**Structure is information.** Numbering, eyebrows, dividers and labels encode something true about the content. Numbered markers fit only a real sequence, such as a process or a timeline whose order the reader needs.

**When the document is a dashboard or status report**, it is scanned rather than read. Put the summary before the detail. Encode state in form as well as in numbers (a pill, a chip, a severity stripe) so what needs attention shows at a glance. Semantic colour (good, warning, critical) is separate from the accent.

## Design for review

A curio exists to be commented on, so its structure is also its review surface.

- Give an `id` to every unit the user might want to discuss: each section, figure, table, decision, option, card, and table rows when rows are the subject. Name it by concept (`capacity-assumptions`, `option-queue-writes`), never by position (`section-3`).
- When an element's text would make a poor anchor label (a figure, a row of numbers), give it an `aria-label` that names it.
- Prefer several small anchored units over one large one, so a comment lands on the claim it is about.
- Keep IDs when restyling or reordering, and never reuse a removed ID for a different concept: earlier comments still point at it.

## Diagrams

Draw as the engineer who has to live with the decision, not as a decorator. A diagram earns its place when it lets a cold reader see a mechanism they would otherwise assemble from prose: where data flows, which components talk, what changes between two options, what states a request moves through. If a sentence says it faster, write the sentence.

**Depict the mechanism, not its name.** A box labelled "cache" says less than the prose; the path a request takes through it, the two stores it sits between and the arrow that disappears without it say what words can't. Show the parts the argument turns on and leave out the rest.

**Comparing options? Draw the difference.** Side by side, before and after, the one edge each option adds or removes: the reader should be able to point at what they are choosing between. Give each option's figure its own `id` so the user can comment on it.

**Match complexity to the stakes**, and **label the arrows**: `writes`, `invalidates`, `polls every 30s`. Add a legend only when the same encoding repeats.

Hand-author inline `<svg>` with native shapes (`rect`, `circle`, `line`, `polyline`, `path`) and `<text>`:

- Size it by `viewBox="0 0 W H"`, chosen for the content, and let CSS scale it (`max-width: 100%; height: auto`). Wide flows read left to right; layered stacks top to bottom.
- Draw strokes, text and arrowheads in `currentColor` so they follow the theme; keep one token colour for the element that carries meaning.
- Arrowheads are a `<marker>` in `<defs>` referenced by `marker-end="url(#arrow)"`, or a small `<polygon>`. Marker ids must be unique in the whole page, so suffix them per figure.
- Keep text around 11-13 px at the drawn scale, with short labels; explanation goes in the caption.
- Align to a grid: shared baselines and even gaps make a hand diagram read as deliberate.
- One figure, one claim: wrap the `<svg>` in a `<figure id="...">` with a `<figcaption>` that states what it shows, and give the `<svg>` `role="img"` and an `aria-label` with the same claim.

## Process

Before writing, settle a compact token system and write it as the `:root` block at the top of the `<style>`: 4-6 named colour tokens and font tokens for at least two roles. Keep the layout concept in mind rather than in a comment, since CSS comments are refused. Build the rest from those tokens. Unless the user asks about the design, one plain sentence on the direction is the most to say about it.

Then write, publish and show, as the curios skill says: no preview loop. Review happens in the viewer, and further polish is for the user to ask for in comments.

## When the brief is editorial

The client has already rejected proposals that felt templated and wants a distinctive point of view. Make opinionated decisions and take one real aesthetic risk where it serves the work. Check the token plan against the subject before building: anything that reads like the default you would produce for any similar document gets revised in the plan itself.

- The opening states the thesis: the most characteristic thing in the subject's world.
- Typography sets the personality: pair the stacks deliberately, with a clear scale and intentional weights, widths and spacing, so the type treatment itself is memorable.
- Use motion sparingly, if at all: CSS only, from a visible resting state, off under reduced motion.
- Match complexity to the vision: maximal directions need elaborate execution, minimal ones precision in spacing, type and detail.
- Put the boldness in one place and keep everything around it quiet.
