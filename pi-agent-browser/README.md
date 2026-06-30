# agent-browser pi extension

A pi extension that exposes `agent-browser` as tools I can call during a session.

## Prerequisites

```bash
npm install -g agent-browser
agent-browser install        # download Chrome for Testing
agent-browser doctor         # verify the install
```

## Wiring it up

The project-local `.pi/settings.json` already registers this extension with a relative path, so it loads automatically whenever pi is opened in this project — no absolute paths, no symlinks, no container-specific setup needed.

If you want it available globally in every project, add it to `~/.pi/agent/settings.json` with an absolute path:

```json
{
  "extensions": ["/path/to/agent-browser/pi-agent-browser"]
}
```

Then reload pi with `/reload` or restart it.

## Tools

| Tool | Description |
|------|-------------|
| `browser_open` | Launch the browser and navigate to a URL |
| `browser_navigate` | Navigate an already-open browser to a new URL |
| `browser_back` | Go back to the previous page |
| `browser_reload` | Reload the current page |
| `browser_close` | Close the browser |
| `browser_snapshot` | Get the accessibility tree with element refs (`@e1`, `@e2`, ...) |
| `browser_screenshot` | Take a screenshot; `annotate=true` overlays numbered labels |
| `browser_read` | Fetch page content as markdown/text |
| `browser_get` | Get a property from an element or the page (text, url, title, ...) |
| `browser_click` | Click an element by ref or selector |
| `browser_fill` | Clear and fill an input field |
| `browser_press` | Press a key or key combination |
| `browser_select` | Select a dropdown option |
| `browser_scroll` | Scroll the page or an element |
| `browser_hover` | Hover over an element |
| `browser_wait` | Wait for an element, text, URL pattern, load state, or time |
| `browser_eval` | Run JavaScript in the page and return the result |
| `browser_is` | Check whether an element is visible, enabled, or checked |

## Typical workflow

```
browser_open url=https://example.com
browser_snapshot interactive_only=true
  -> heading "Example Domain" [ref=e1]
  -> link "More information..." [ref=e2]
browser_click selector=@e2
browser_wait load=networkidle
browser_read
browser_close
```

Refs (`@e1`, `@e2`) come from `browser_snapshot` and are valid until the page changes.
After any navigation or significant DOM update, take a fresh snapshot before using refs again.
