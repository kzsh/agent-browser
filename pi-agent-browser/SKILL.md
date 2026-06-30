---
name: agent-browser
description: Browser automation tools for navigating pages, reading content, filling forms, and taking screenshots. Use when a task requires interacting with a website or reading live web content.
---

# agent-browser

Browser automation via the `agent-browser` CLI. The tools wrap the CLI and return JSON.

## Workflow

Always follow this sequence:

1. `browser_open` — launch and navigate to the URL
2. `browser_snapshot` — get the accessibility tree and element refs
3. Interact using refs (`@e1`, `@e2`, ...) from the snapshot
4. `browser_close` — clean up when done

## Prefer text over images

Take a `browser_screenshot` only when visual layout genuinely matters — for example, a canvas element, a chart, an image-heavy page, or a UI bug that cannot be described in text. For most tasks the accessibility tree (`browser_snapshot`) or page text (`browser_read`) is faster and gives the model more precise, copy-pasteable content to work with.

Good reasons to take a screenshot:
- The page uses canvas, SVG, or other non-text rendering
- You need to confirm visual layout or styling
- An element is not reachable via the accessibility tree
- You are debugging a rendering problem

Poor reasons to take a screenshot:
- "To see what the page looks like" when `browser_snapshot` already shows the structure
- Reading text content that `browser_read` would return directly
- Confirming a click worked when the snapshot already reflects the new state

Annotated screenshots (`annotate=true`) are useful when visual context and element refs are both needed at once.

## Choosing between snapshot and read

- `browser_snapshot` — for interacting with the page (finding buttons, inputs, links)
- `browser_read` — for extracting text content (articles, docs, data)

Use `browser_snapshot interactive_only=true` to cut noise when you only need to find things to click or fill. Use `browser_read` without a URL to read the rendered DOM of the active tab, including content loaded by JavaScript.

## Refs

Element refs (`@e1`, `@e2`, ...) are only valid for the snapshot they came from. After any navigation or significant DOM change, take a fresh snapshot before using refs again.

If a click is blocked by a covering element (consent banner, modal), dismiss it first, then re-snapshot before retrying the original action.

## Waiting

After clicks that trigger navigation or async updates, use `browser_wait` before the next snapshot. Prefer `load=networkidle` for full-page transitions, `text=` for confirming specific content appeared, and `selector=` for confirming a specific element is ready.
