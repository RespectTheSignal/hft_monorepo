// Transparent proxy for api.flipster.io.
// Routes every request through Cloudflare's network so the origin
// (api.flipster.io, also a CF customer) sees the request as coming
// from CF's edge — same trust class as the official web client. The
// caller (our executor) just hits this Worker URL with the same path
// + headers + body it would send to api.flipster.io directly.
//
// Auth shared-secret guard: pass `X-Worker-Auth: <SECRET>` so random
// scrapers can't piggyback on our IP cooldown if the URL leaks.
//
// Returns the upstream response 1:1 (status, headers, body) so the
// executor's existing parsing keeps working.

const UPSTREAM = "https://api.flipster.io";
const SECRET = "fl-worker-Th3pFmK6wQ"; // shared secret, set in executor too

export default {
  async fetch(request) {
    if ((request.headers.get("X-Worker-Auth") || "") !== SECRET) {
      return new Response("forbidden", { status: 403 });
    }
    const url = new URL(request.url);
    const upstream = new URL(UPSTREAM);
    upstream.pathname = url.pathname;
    upstream.search = url.search;

    // Strip Cloudflare-specific request headers + our auth header
    // before forwarding. Keep cookies / content-type / user-agent so
    // the upstream sees a normal-looking client.
    const fwd = new Headers(request.headers);
    fwd.delete("X-Worker-Auth");
    fwd.delete("X-Forwarded-For");
    fwd.delete("X-Real-IP");
    fwd.delete("CF-Connecting-IP");
    fwd.delete("CF-IPCountry");
    fwd.delete("CF-Ray");
    fwd.delete("CF-Visitor");
    fwd.set("Host", upstream.host);
    fwd.set("Origin", "https://flipster.io");

    const init = {
      method: request.method,
      headers: fwd,
      redirect: "manual",
    };
    if (!["GET", "HEAD"].includes(request.method)) {
      init.body = await request.arrayBuffer();
    }

    const resp = await fetch(upstream.toString(), init);
    const body = await resp.arrayBuffer();
    return new Response(body, {
      status: resp.status,
      statusText: resp.statusText,
      headers: resp.headers,
    });
  },
};
