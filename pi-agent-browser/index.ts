/**
 * agent-browser extension for pi
 *
 * Wraps the agent-browser CLI as pi tools. The standard workflow is:
 *   1. browser_open      — navigate to a URL
 *   2. browser_snapshot  — get the accessibility tree with element refs (@e1, @e2, ...)
 *   3. browser_click / browser_fill / browser_press — interact using those refs
 *   4. browser_screenshot / browser_read — observe results
 *   5. browser_close     — clean up
 *
 * Wire it up by adding this directory to ~/.pi/agent/settings.json:
 *   { "extensions": ["/absolute/path/to/pi-agent-browser"] }
 *
 * Or symlink it:
 *   ln -s /absolute/path/to/pi-agent-browser ~/.pi/agent/extensions/agent-browser
 */

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

const execFileAsync = promisify(execFile);

/** Check whether the agent-browser binary is on PATH. */
async function isInstalled(): Promise<boolean> {
  try {
    await execFileAsync("agent-browser", ["--version"], { timeout: 5_000 });
    return true;
  } catch (err: unknown) {
    const e = err as { code?: string };
    return e.code !== "ENOENT";
  }
}

/**
 * Run an agent-browser command with --json output.
 * agent-browser exits non-zero on errors but still writes JSON to stdout,
 * so we surface that rather than letting execFile swallow it as a throw.
 */
async function ab(...args: string[]): Promise<string> {
  try {
    const { stdout } = await execFileAsync("agent-browser", [...args, "--json"], {
      timeout: 35_000,
    });
    return stdout.trim();
  } catch (err: unknown) {
    const e = err as { code?: string; stdout?: string; stderr?: string; message?: string };
    if (e.code === "ENOENT") {
      return JSON.stringify({
        success: false,
        error: "agent-browser is not installed. Run: npm install -g agent-browser && agent-browser install",
      });
    }
    if (e.stdout?.trim()) return e.stdout.trim();
    const msg = e.stderr?.trim() || e.message || "unknown error";
    return JSON.stringify({ success: false, error: msg });
  }
}

function result(raw: string) {
  return { content: [{ type: "text" as const, text: raw }], details: {} };
}

export default function agentBrowserExtension(pi: ExtensionAPI) {
  pi.on("session_start", async (_event, ctx) => {
    if (!(await isInstalled())) {
      ctx.ui.notify(
        "agent-browser is not installed — browser tools are unavailable. Run: npm install -g agent-browser && agent-browser install",
        "warning",
      );
    }
  });
  // ---------------------------------------------------------------------------
  // Navigation
  // ---------------------------------------------------------------------------

  pi.registerTool({
    name: "browser_open",
    label: "Browser: Open",
    description:
      "Launch the browser and navigate to a URL. Call this first before any other browser tools.",
    promptSnippet: "Open a URL in the browser",
    parameters: Type.Object({
      url: Type.String({ description: "URL to navigate to (e.g. https://example.com)" }),
    }),
    async execute(_id, { url }) {
      return result(await ab("open", url));
    },
  });

  pi.registerTool({
    name: "browser_close",
    label: "Browser: Close",
    description: "Close the browser and end the session.",
    promptSnippet: "Close the browser",
    parameters: Type.Object({}),
    async execute() {
      return result(await ab("close"));
    },
  });

  pi.registerTool({
    name: "browser_navigate",
    label: "Browser: Navigate",
    description:
      "Navigate the already-open browser to a new URL without relaunching it.",
    parameters: Type.Object({
      url: Type.String({ description: "URL to navigate to" }),
    }),
    async execute(_id, { url }) {
      return result(await ab("goto", url));
    },
  });

  pi.registerTool({
    name: "browser_back",
    label: "Browser: Back",
    description: "Go back to the previous page.",
    parameters: Type.Object({}),
    async execute() {
      return result(await ab("back"));
    },
  });

  pi.registerTool({
    name: "browser_reload",
    label: "Browser: Reload",
    description: "Reload the current page.",
    parameters: Type.Object({}),
    async execute() {
      return result(await ab("reload"));
    },
  });

  // ---------------------------------------------------------------------------
  // Observation
  // ---------------------------------------------------------------------------

  pi.registerTool({
    name: "browser_snapshot",
    label: "Browser: Snapshot",
    description:
      "Get the accessibility tree for the current page. Returns element refs like @e1, @e2 " +
      "that can be passed to browser_click, browser_fill, etc. " +
      "Always take a fresh snapshot before interacting with elements.",
    promptSnippet: "Get the page accessibility tree with element refs",
    parameters: Type.Object({
      interactive_only: Type.Optional(
        Type.Boolean({
          description:
            "Only return interactive elements (buttons, inputs, links). Reduces noise. Recommended.",
        })
      ),
      selector: Type.Optional(
        Type.String({ description: "Scope the snapshot to a CSS selector" })
      ),
      depth: Type.Optional(
        Type.Number({ description: "Limit tree depth (e.g. 3)" })
      ),
    }),
    async execute(_id, { interactive_only, selector, depth }) {
      const args = ["snapshot"];
      if (interactive_only) args.push("-i");
      if (selector) args.push("-s", selector);
      if (depth != null) args.push("-d", String(depth));
      return result(await ab(...args));
    },
  });

  pi.registerTool({
    name: "browser_screenshot",
    label: "Browser: Screenshot",
    description:
      "Take a screenshot of the current page and return the file path. " +
      "Use annotate=true to overlay numbered labels that match element refs.",
    promptSnippet: "Take a page screenshot",
    parameters: Type.Object({
      annotate: Type.Optional(
        Type.Boolean({
          description:
            "Overlay numbered [N] labels on interactive elements. Each label corresponds to ref @eN.",
        })
      ),
      full_page: Type.Optional(
        Type.Boolean({ description: "Capture the full scrollable page, not just the viewport." })
      ),
    }),
    async execute(_id, { annotate, full_page }) {
      const args = ["screenshot"];
      if (annotate) args.push("--annotate");
      if (full_page) args.push("--full");
      return result(await ab(...args));
    },
  });

  pi.registerTool({
    name: "browser_read",
    label: "Browser: Read",
    description:
      "Fetch agent-readable markdown/text content from a URL, or read the rendered DOM of the " +
      "active tab (omit url). Prefer this over browser_snapshot when you need the page text content.",
    promptSnippet: "Read page content as markdown text",
    parameters: Type.Object({
      url: Type.Optional(
        Type.String({
          description: "URL to read. Omit to read the active tab's rendered content.",
        })
      ),
      filter: Type.Optional(
        Type.String({ description: "Narrow to sections matching this text" })
      ),
    }),
    async execute(_id, { url, filter }) {
      const args = url ? ["read", url] : ["read"];
      if (filter) args.push("--filter", filter);
      return result(await ab(...args));
    },
  });

  pi.registerTool({
    name: "browser_get",
    label: "Browser: Get",
    description:
      "Get a specific property from an element or the page. " +
      "property can be: text, html, value, title, url, count, box.",
    parameters: Type.Object({
      property: Type.Union(
        [
          Type.Literal("text"),
          Type.Literal("html"),
          Type.Literal("value"),
          Type.Literal("title"),
          Type.Literal("url"),
          Type.Literal("count"),
          Type.Literal("box"),
        ],
        { description: "Property to retrieve" }
      ),
      selector: Type.Optional(
        Type.String({
          description:
            "Element ref (e.g. @e1) or CSS selector. Required for text, html, value, count, box.",
        })
      ),
    }),
    async execute(_id, { property, selector }) {
      const args = ["get", property];
      if (selector) args.push(selector);
      return result(await ab(...args));
    },
  });

  // ---------------------------------------------------------------------------
  // Interaction
  // ---------------------------------------------------------------------------

  pi.registerTool({
    name: "browser_click",
    label: "Browser: Click",
    description:
      "Click an element. Use refs from browser_snapshot (e.g. @e2) for reliable targeting. " +
      "If click is blocked by an overlay, dismiss it first then take a fresh snapshot.",
    promptSnippet: "Click an element by ref or selector",
    parameters: Type.Object({
      selector: Type.String({
        description: "Element ref like @e2, or a CSS selector / text=... / xpath=...",
      }),
    }),
    async execute(_id, { selector }) {
      return result(await ab("click", selector));
    },
  });

  pi.registerTool({
    name: "browser_fill",
    label: "Browser: Fill",
    description: "Clear an input element and fill it with text.",
    promptSnippet: "Fill an input field",
    parameters: Type.Object({
      selector: Type.String({ description: "Element ref or CSS selector" }),
      text: Type.String({ description: "Text to fill" }),
    }),
    async execute(_id, { selector, text }) {
      return result(await ab("fill", selector, text));
    },
  });

  pi.registerTool({
    name: "browser_press",
    label: "Browser: Press",
    description:
      "Press a key or key combination (e.g. Enter, Tab, Control+a, ArrowDown).",
    parameters: Type.Object({
      key: Type.String({ description: "Key name or combo, e.g. Enter, Tab, Control+a" }),
    }),
    async execute(_id, { key }) {
      return result(await ab("press", key));
    },
  });

  pi.registerTool({
    name: "browser_select",
    label: "Browser: Select",
    description: "Select an option from a <select> dropdown element.",
    parameters: Type.Object({
      selector: Type.String({ description: "Element ref or CSS selector for the <select>" }),
      value: Type.String({ description: "Option value or label to select" }),
    }),
    async execute(_id, { selector, value }) {
      return result(await ab("select", selector, value));
    },
  });

  pi.registerTool({
    name: "browser_scroll",
    label: "Browser: Scroll",
    description: "Scroll the page or an element.",
    parameters: Type.Object({
      direction: Type.Union(
        [
          Type.Literal("up"),
          Type.Literal("down"),
          Type.Literal("left"),
          Type.Literal("right"),
        ],
        { description: "Scroll direction" }
      ),
      pixels: Type.Optional(
        Type.Number({ description: "How many pixels to scroll (default: 300)" })
      ),
      selector: Type.Optional(
        Type.String({ description: "Scroll a specific element instead of the page" })
      ),
    }),
    async execute(_id, { direction, pixels, selector }) {
      const args = ["scroll", direction];
      if (pixels != null) args.push(String(pixels));
      if (selector) args.push("--selector", selector);
      return result(await ab(...args));
    },
  });

  pi.registerTool({
    name: "browser_hover",
    label: "Browser: Hover",
    description: "Hover over an element (useful for revealing tooltips or dropdown menus).",
    parameters: Type.Object({
      selector: Type.String({ description: "Element ref or CSS selector" }),
    }),
    async execute(_id, { selector }) {
      return result(await ab("hover", selector));
    },
  });

  // ---------------------------------------------------------------------------
  // Waiting
  // ---------------------------------------------------------------------------

  pi.registerTool({
    name: "browser_wait",
    label: "Browser: Wait",
    description:
      "Wait for a condition before continuing. Useful after clicks that trigger navigation or " +
      "async content. Specify exactly one of: selector, text, url, load, ms.",
    parameters: Type.Object({
      selector: Type.Optional(
        Type.String({ description: "Wait for this element to be visible" })
      ),
      text: Type.Optional(
        Type.String({ description: "Wait for this text to appear on the page" })
      ),
      url: Type.Optional(
        Type.String({ description: "Wait for the URL to match this pattern (supports **)" })
      ),
      load: Type.Optional(
        Type.Union(
          [
            Type.Literal("load"),
            Type.Literal("domcontentloaded"),
            Type.Literal("networkidle"),
          ],
          { description: "Wait for a page load state" }
        )
      ),
      ms: Type.Optional(
        Type.Number({ description: "Wait for this many milliseconds" })
      ),
    }),
    async execute(_id, { selector, text, url, load, ms }) {
      if (selector) return result(await ab("wait", selector));
      if (text) return result(await ab("wait", "--text", text));
      if (url) return result(await ab("wait", "--url", url));
      if (load) return result(await ab("wait", "--load", load));
      if (ms != null) return result(await ab("wait", String(ms)));
      return result(JSON.stringify({ success: false, error: "Provide one of: selector, text, url, load, ms" }));
    },
  });

  // ---------------------------------------------------------------------------
  // JavaScript
  // ---------------------------------------------------------------------------

  pi.registerTool({
    name: "browser_eval",
    label: "Browser: Eval",
    description: "Run JavaScript in the page context and return the result.",
    parameters: Type.Object({
      script: Type.String({ description: "JavaScript expression or statement to evaluate" }),
    }),
    async execute(_id, { script }) {
      return result(await ab("eval", script));
    },
  });

  // ---------------------------------------------------------------------------
  // State checks
  // ---------------------------------------------------------------------------

  pi.registerTool({
    name: "browser_is",
    label: "Browser: Is",
    description: "Check whether an element is visible, enabled, or checked.",
    parameters: Type.Object({
      state: Type.Union(
        [Type.Literal("visible"), Type.Literal("enabled"), Type.Literal("checked")],
        { description: "State to check" }
      ),
      selector: Type.String({ description: "Element ref or CSS selector" }),
    }),
    async execute(_id, { state, selector }) {
      return result(await ab("is", state, selector));
    },
  });
}
