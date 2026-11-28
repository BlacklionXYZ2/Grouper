// Small self-contained HTML pages. No external assets.

const STYLE = `
:root{color-scheme:light dark;--bg:#fff;--fg:#1a1a1a;--mut:#666;--card:#f4f4f5;--btn:#1a1a1a;--btnfg:#fff}
@media (prefers-color-scheme:dark){:root{--bg:#111113;--fg:#f0f0f0;--mut:#9a9a9f;--card:#1c1c1f;--btn:#f0f0f0;--btnfg:#111113}}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);
font:16px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif;padding:16px}
main{max-width:360px;width:100%;background:var(--card);border-radius:14px;padding:28px 24px;text-align:center}
h1{font-size:20px;margin:0 0 8px}
p{color:var(--mut);margin:0 0 20px}
a.btn{display:inline-block;background:var(--btn);color:var(--btnfg);text-decoration:none;
padding:10px 18px;border-radius:8px;font-weight:600}
`;

function page(title: string, heading: string, text: string, button?: { href: string; label: string }): string {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>${title}</title><style>${STYLE}</style></head>
<body><main><h1>${heading}</h1><p>${text}</p>${
    button ? `<a class="btn" href="${button.href}">${button.label}</a>` : ""
  }</main></body></html>`;
}

export function signInPage(): Response {
  return html(
    200,
    page(
      "Grouper relay",
      "Grouper",
      "Sign in with the Google account you use with your hub.",
      { href: "/relay/auth/start", label: "Sign in with Google" },
    ),
  );
}

export function hubOfflinePage(): Response {
  return html(
    503,
    page("Hub offline", "Hub not connected", "Your hub isn't connected to the relay. Check that it is running and linked, then reload."),
  );
}

export function errorPage(status: number, message: string): Response {
  return html(
    status,
    page("Grouper relay", "Couldn't sign in", message.replace(/[<>&"]/g, ""), { href: "/relay/auth/start", label: "Try again" }),
  );
}

function html(status: number, body: string): Response {
  return new Response(body, {
    status,
    headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" },
  });
}
