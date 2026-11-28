// Small HTML pages the Worker renders itself. They use the website's stylesheet (/site/style.css)
// and logo so they match the site; no external assets.

function esc(s: string): string {
  return s.replace(/[<>&"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

function page(title: string, heading: string, text: string, button?: { href: string; label: string }, foot?: string): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex">
<title>${esc(title)}</title>
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/site/style.css"></head>
<body>
<header class="top"><div class="wrap"><a class="brand" href="/"><img src="/site/icon.svg" alt="" width="32" height="32">Grouper</a></div></header>
<main class="center"><div class="panel">
<img src="/site/icon.svg" alt="" width="64" height="64">
<h1>${esc(heading)}</h1><p>${esc(text)}</p>${
    button ? `<a class="btn" href="${esc(button.href)}">${esc(button.label)}</a>` : ""
  }${foot ?? ""}</div></main>
</body></html>`;
}

/** `next` must already be a validated same-origin path. */
export function signInPage(next = "/app"): Response {
  const start = "/relay/auth/start" + (next === "/app" ? "" : `?next=${encodeURIComponent(next)}`);
  return html(
    200,
    page("Sign in · Grouper", "Sign in to Grouper", "Sign in with the Google account you use with your hub.", {
      href: start,
      label: "Sign in with Google",
    }),
  );
}

/** Confirmation for the command-line sign-in. The button POSTs; a GET never mints anything. */
export function cliConfirmPage(email: string, port: string, state: string, csrf: string): Response {
  const form = `<form method="post" action="/relay/v1/cli-token">
<input type="hidden" name="port" value="${esc(port)}"><input type="hidden" name="state" value="${esc(state)}"><input type="hidden" name="csrf" value="${esc(csrf)}">
<button class="btn" type="submit">Sign in the command line</button></form>
<p style="margin:20px 0 0;font-size:14px">Only continue if you just ran <code>grouper login</code>. If not, close this tab.</p>`;
  const r = html(200, page("Command line sign-in · Grouper", "Sign in the command line?", `Sign the Grouper command line on this computer in as ${email}?`, undefined, form));
  r.headers.set("content-security-policy", "frame-ancestors 'none'");
  r.headers.set("x-frame-options", "DENY");
  return r;
}

export function hubOfflinePage(): Response {
  return html(
    503,
    page("Hub offline · Grouper", "Hub not connected", "Your hub isn't connected to the relay. Check that it is running and linked, then reload."),
  );
}

export function errorPage(status: number, message: string, retry = "/relay/auth/start"): Response {
  return html(status, page("Sign in failed · Grouper", "Couldn't sign in", message, { href: retry, label: "Try again" }));
}

function html(status: number, body: string): Response {
  return new Response(body, {
    status,
    headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" },
  });
}
