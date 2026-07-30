/**
 * netproxy.js — API proxy running inside a hidden web.groupme.com webview.
 *
 * Why this exists: on a network with a filtering proxy that allowlists the
 * GroupMe *web app* but not the bare API host (Techloq was the observed case),
 * the Rust worker's direct api.groupme.com calls are answered with the filter's
 * block page while the identical request from a web.groupme.com page succeeds.
 * The difference is the browser context — origin and TLS fingerprint — which no
 * header can reproduce. So when Rust detects interception it hands the fetch to
 * this script, which runs on the real web.groupme.com origin and makes the
 * request the filter already permits. Same account, same data the browser shows.
 *
 * This window is created lazily, only after interception is seen, so users who
 * are not filtered never pay for a second webview.
 *
 * Protocol, over the Tauri event channel (this origin holds core:event:* only):
 *   Rust -> here:  netproxy://fetch   { id, url, token }
 *   here -> Rust:  netproxy://result  { id, ok, status, body } | { id, ok:false, error }
 *   here -> Rust:  netproxy://ready   (once the listener is attached)
 */
(function () {
  'use strict';

  // initialization_script runs on every navigation; the SPA navigates. Guard so
  // only one listener is ever attached, or every reply would be sent N times.
  if (window.__netproxyInstalled) { return; }
  window.__netproxyInstalled = true;

  // Capture the NATIVE fetch now, at document-start, before GroupMe's SPA loads
  // and replaces window.fetch with its own wrapper. Fetching through the SPA's
  // fetch (or its service worker) was what turned a working request into
  // "Failed to fetch" — the API is reachable from this webview, as a plain
  // app-page request to it returns a real 401. Use the untouched implementation.
  var nativeFetch = window.fetch.bind(window);

  // And take the service worker out of the path entirely: a controlling SW can
  // intercept these cross-origin requests and fail them.
  try {
    if (navigator.serviceWorker && navigator.serviceWorker.getRegistrations) {
      navigator.serviceWorker.getRegistrations().then(function (rs) {
        rs.forEach(function (r) { r.unregister(); });
      }).catch(function () {});
    }
  } catch (_) {}

  function ev() {
    return (window.__TAURI__ && window.__TAURI__.event) || null;
  }
  function emit(name, payload) {
    try {
      var e = ev();
      if (e) { e.emit(name, payload).catch(function () {}); }
    } catch (_) { /* never throw into the page */ }
  }

  function diag(msg) { emit('netproxy://diag', { msg: String(msg).slice(0, 400) }); }

  var e = ev();
  if (e) {
    e.listen('netproxy://fetch', function (msg) {
      var p = (msg && msg.payload) || {};
      // Token header only, no cookies. GroupMe's API answers with a wildcard
      // `Access-Control-Allow-Origin`, which a browser rejects if credentials
      // are included — so credentials:'omit' is required, and the token header
      // authenticates on its own (verified against the live API).
      nativeFetch(p.url, {
        headers: { 'x-access-token': p.token },
        credentials: 'omit',
        redirect: 'follow'
      }).then(function (r) {
        return r.text().then(function (body) {
          emit('netproxy://result', { id: p.id, ok: true, status: r.status, body: body });
        });
      }).catch(function (err) {
        diag('fetch failed at ' + location.href + ': ' + err);
        emit('netproxy://result', { id: p.id, ok: false, error: String(err) });
      });
    });
  }

  // Announce readiness only after the listener above is attached, so Rust never
  // emits a fetch into a window that cannot yet hear it.
  emit('netproxy://ready', {});
}());
