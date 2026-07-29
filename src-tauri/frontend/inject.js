/**
 * inject.js — GroupMe Desktop token-capture initialization script.
 *
 * Injected into every webview frame before page scripts run (Tauri 2
 * initialization_script). Intercepts outgoing API requests to extract the
 * user's GroupMe access token and forward it to Rust via the Tauri event
 * channel, so the background archive worker can authenticate.
 *
 * WHY the request header and NOT localStorage:
 *   The x-access-token request header is the wire contract between GroupMe's
 *   web client and api.groupme.com. It is not a private implementation detail —
 *   any browser with DevTools open can see it. localStorage key names, by
 *   contrast, are minified build output; GroupMe can and does rename or
 *   restructure them between deploys without notice. Intercepting the header
 *   never silently breaks; reading a renamed localStorage key silently returns
 *   undefined forever.
 *
 * Delivery: the Tauri remote capability for web.groupme.com grants only
 * core:event:allow-emit / allow-listen, so we use the EVENT channel. invoke()
 * is not available to remote origins.
 */

(function () {
  'use strict';

  // ── Double-injection guard ──────────────────────────────────────────────
  // initialization_script runs once per navigation per frame, but guard
  // anyway in case Tauri ever changes that guarantee.
  if (window.__groupmeDesktopInjected) { return; }
  window.__groupmeDesktopInjected = true;

  // ── State ────────────────────────────────────────────────────────────────
  // Track the last-emitted token so we don't spam IPC on every request.
  var lastSentToken = null;

  // ── Tauri IPC helper ─────────────────────────────────────────────────────
  // Silently no-ops when __TAURI__ is absent (plain browser / dev mode).
  function tauriEmit(event, payload) {
    try {
      if (window.__TAURI__ && window.__TAURI__.event) {
        // emit() returns a Promise; swallow rejection so we never throw.
        window.__TAURI__.event.emit(event, payload).catch(function () {});
      }
    } catch (e) {
      // Our instrumentation must never surface errors to the page.
    }
  }

  // ── Token delivery ───────────────────────────────────────────────────────
  function sendToken(token) {
    if (!token || token === lastSentToken) { return; }
    lastSentToken = token;
    // Log ONLY the fact of capture, never the token value itself.
    console.log('[groupme-desktop] access token captured');
    tauriEmit('groupme://token', { token: token });
  }

  // ── URL helpers ──────────────────────────────────────────────────────────
  function isGroupMeApiUrl(url) {
    try {
      var s = (url && typeof url.toString === 'function') ? url.toString() : String(url);
      return s.indexOf('api.groupme.com') !== -1;
    } catch (e) {
      return false;
    }
  }

  // Extracts token= from a query string without relying on URLSearchParams
  // (which is ES6 — fine in WebView2, but kept simple for portability).
  function tokenFromQueryString(url) {
    try {
      var s = (url && typeof url.toString === 'function') ? url.toString() : String(url);
      var match = s.match(/[?&]token=([A-Za-z0-9]+)/);
      return match ? match[1] : null;
    } catch (e) {
      return null;
    }
  }

  // ── Header extraction helpers ─────────────────────────────────────────────
  // Handles both the Headers object (which normalises names to lowercase) and
  // plain objects (which preserve whatever case the caller used).
  function tokenFromHeaders(headers) {
    if (!headers) { return null; }
    try {
      if (typeof headers.get === 'function') {
        // Headers object: names are already case-folded to lowercase.
        return headers.get('x-access-token') || null;
      }
      // Plain object: search case-insensitively.
      var keys = Object.keys(headers);
      for (var i = 0; i < keys.length; i++) {
        if (keys[i].toLowerCase() === 'x-access-token') {
          return headers[keys[i]] || null;
        }
      }
    } catch (e) {
      // Malformed headers object — ignore.
    }
    return null;
  }

  // ── fetch patch ───────────────────────────────────────────────────────────
  // GroupMe's React client uses fetch for most API calls. We intercept before
  // the call is made so we see the headers exactly as the app intended to send
  // them.
  var _originalFetch = window.fetch;
  if (typeof _originalFetch === 'function') {
    window.fetch = function fetch(resource, init) {
      try {
        // resource can be a URL string, a URL object, or a Request object.
        var url = (resource && resource.url != null) ? resource.url : resource;

        if (isGroupMeApiUrl(url)) {
          var token = null;

          // Primary: header from the init object.
          if (init && init.headers) {
            token = tokenFromHeaders(init.headers);
          }

          // Secondary: header on a pre-built Request object.
          if (!token && resource && typeof resource.headers !== 'undefined') {
            token = tokenFromHeaders(resource.headers);
          }

          // Tertiary: token= query parameter.
          if (!token) {
            token = tokenFromQueryString(url);
          }

          if (token) { sendToken(token); }
        }
      } catch (e) {
        // Our instrumentation must not prevent the request from being made.
      }

      // Always delegate to the real fetch, preserving `this` and all args.
      return _originalFetch.apply(this, arguments);
    };
  }

  // ── XMLHttpRequest patch ──────────────────────────────────────────────────
  // GroupMe's client is old enough that some code paths may still use XHR
  // (bundled polyfills, older utilities). Patch the three lifecycle methods.
  //
  // Strategy: store the target URL on the XHR instance at open(), then
  // capture the token at setRequestHeader() when we see x-access-token, and
  // fall back to a URL param scan at send() if no header was seen.
  if (window.XMLHttpRequest) {
    var _xhrOpen = XMLHttpRequest.prototype.open;
    var _xhrSetRequestHeader = XMLHttpRequest.prototype.setRequestHeader;
    var _xhrSend = XMLHttpRequest.prototype.send;

    XMLHttpRequest.prototype.open = function open(method, url) {
      try {
        // Stash the URL so setRequestHeader / send can inspect it.
        this.__groupmeUrl = url;
        this.__groupmeTokenSeen = false;
      } catch (e) {}
      return _xhrOpen.apply(this, arguments);
    };

    XMLHttpRequest.prototype.setRequestHeader = function setRequestHeader(name, value) {
      try {
        if (
          !this.__groupmeTokenSeen &&
          typeof name === 'string' &&
          name.toLowerCase() === 'x-access-token' &&
          isGroupMeApiUrl(this.__groupmeUrl)
        ) {
          this.__groupmeTokenSeen = true;
          sendToken(value);
        }
      } catch (e) {}
      return _xhrSetRequestHeader.apply(this, arguments);
    };

    XMLHttpRequest.prototype.send = function send() {
      try {
        // Fallback: if no header was intercepted, try the URL query param.
        if (!this.__groupmeTokenSeen && isGroupMeApiUrl(this.__groupmeUrl)) {
          var token = tokenFromQueryString(this.__groupmeUrl);
          if (token) { sendToken(token); }
        }
      } catch (e) {}
      return _xhrSend.apply(this, arguments);
    };
  }

  // ── Online / offline events ───────────────────────────────────────────────
  // Rust can react immediately instead of waiting for its own probe tick.
  window.addEventListener('online', function () {
    tauriEmit('groupme://online', {});
  });

  window.addEventListener('offline', function () {
    tauriEmit('groupme://offline', {});
  });

  // ── Surface toggle ────────────────────────────────────────────────────────
  // A small fixed button that switches to the app's own client UI. It goes
  // over the EVENT channel because a remote origin cannot invoke commands;
  // Rust re-validates the payload, records the preference, and navigates.
  // Top frame of web.groupme.com only — never inside iframes or local pages.
  (function addUiToggle() {
    try {
      if (window.top !== window) { return; }
      if (location.host !== 'web.groupme.com') { return; }

      function mount() {
        if (document.getElementById('groupme-desktop-uiswap')) { return; }
        if (!document.body) { return; }
        var btn = document.createElement('button');
        btn.id = 'groupme-desktop-uiswap';
        btn.type = 'button';
        btn.textContent = 'Custom UI';
        btn.title = 'Switch to the desktop app’s own client (remembered as your preference)';
        btn.style.cssText =
          'position:fixed;right:14px;bottom:14px;z-index:2147483647;' +
          'font:600 11px/1 system-ui,sans-serif;letter-spacing:.04em;text-transform:uppercase;' +
          'padding:7px 10px;border-radius:8px;border:1px solid rgba(255,255,255,.25);' +
          'background:rgba(20,20,28,.82);color:#fff;cursor:pointer;opacity:.55;';
        btn.addEventListener('mouseenter', function () { btn.style.opacity = '1'; });
        btn.addEventListener('mouseleave', function () { btn.style.opacity = '.55'; });
        btn.addEventListener('click', function () {
          tauriEmit('groupme://switch-ui', { ui: 'client' });
        });
        document.body.appendChild(btn);
      }

      if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', mount);
      } else {
        mount();
      }
    } catch (e) {
      // Cosmetic feature; never let it break the page or the token capture.
    }
  }());

}());
