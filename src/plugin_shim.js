// AntiCraft plugin:// shim (SPEC §4.4 deviation).
//
// WebView2 (through wry) serves the `plugin` custom protocol as
// https://plugin.<authority>/... . This shim rewrites literal
// plugin://<authority>/... URLs used by the page so they reach that handler.
(function () {
  'use strict';
  if (window.__anticraftPluginShim) return;
  window.__anticraftPluginShim = true;

  var PREFIX = /^plugin:\/\/([^\/?#]+)/i;
  function rewrite(u) {
    if (typeof u !== 'string') return u;
    return u.replace(PREFIX, function (_m, authority) {
      return 'https://plugin.' + authority;
    });
  }
  window.__anticraftRewritePluginUrl = rewrite;

  var origFetch = window.fetch;
  if (typeof origFetch === 'function') {
    window.fetch = function (input, init) {
      if (typeof input === 'string') {
        input = rewrite(input);
      } else if (input && typeof input.url === 'string' && PREFIX.test(input.url)) {
        input = new Request(rewrite(input.url), input);
      }
      return origFetch.call(this, input, init);
    };
  }

  var origOpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (method, url) {
    var args = Array.prototype.slice.call(arguments);
    args[1] = rewrite(typeof url === 'string' ? url : String(url));
    return origOpen.apply(this, args);
  };

  var origWindowOpen = window.open;
  window.open = function (url) {
    var args = Array.prototype.slice.call(arguments);
    if (typeof url === 'string') args[0] = rewrite(url);
    return origWindowOpen.apply(this, args);
  };

  // src/href attributes on elements that load resources.
  function patchProp(proto, prop) {
    var d = proto && Object.getOwnPropertyDescriptor(proto, prop);
    if (!d || !d.set) return;
    Object.defineProperty(proto, prop, {
      configurable: true,
      enumerable: d.enumerable,
      get: d.get,
      set: function (v) { d.set.call(this, rewrite(v)); }
    });
  }
  [
    [window.HTMLIFrameElement, 'src'],
    [window.HTMLImageElement, 'src'],
    [window.HTMLScriptElement, 'src'],
    [window.HTMLLinkElement, 'href'],
    [window.HTMLAnchorElement, 'href'],
    [window.HTMLSourceElement, 'src'],
    [window.HTMLVideoElement, 'src'],
    [window.HTMLAudioElement, 'src']
  ].forEach(function (p) { if (p[0]) patchProp(p[0].prototype, p[1]); });

  var origSetAttribute = Element.prototype.setAttribute;
  Element.prototype.setAttribute = function (name, value) {
    var n = String(name).toLowerCase();
    if (n === 'src' || n === 'href') value = rewrite(String(value));
    return origSetAttribute.call(this, name, value);
  };
})();
