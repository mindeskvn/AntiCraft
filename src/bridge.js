// AntiCraft preload bridge (SPEC §5.1).
//
// Injected as a WebView2 initialization script into the main frame. Recreates
// the contextBridge globals of the original shell with the same names and
// Promise semantics on top of chrome.webview.postMessage. The Rust side answers
// with window.__anticraftIpc.resolve(id, ok, value) and pushes events with
// window.__anticraftIpc.emit(channel, payload).
(function () {
  'use strict';
  if (window.__anticraftIpc) return;
  // Post through WebView2 directly; `window.ipc` belongs to the web UI.
  var cw = window.chrome && window.chrome.webview;
  if (!cw || typeof cw.postMessage !== 'function') return;

  var nextId = 1;
  var pending = new Map();
  var listeners = new Map();
  var post = cw.postMessage.bind(cw);

  function send(channel, args, id) {
    post(JSON.stringify({ id: id || 0, channel: channel, args: args || [] }));
  }

  function invoke(channel) {
    var args = Array.prototype.slice.call(arguments, 1);
    return new Promise(function (resolve, reject) {
      var id = nextId++;
      pending.set(id, { resolve: resolve, reject: reject });
      try {
        send(channel, args, id);
      } catch (e) {
        pending.delete(id);
        reject(e);
      }
    });
  }

  // Fire-and-forget (ipcRenderer.send equivalent).
  function fire(channel) {
    send(channel, Array.prototype.slice.call(arguments, 1), 0);
  }

  function on(channel, cb) {
    if (!listeners.has(channel)) listeners.set(channel, new Set());
    var set = listeners.get(channel);
    var wrapped = function (payload) { cb(payload); };
    set.add(wrapped);
    return function unsubscribe() { set.delete(wrapped); };
  }

  Object.defineProperty(window, '__anticraftIpc', {
    value: Object.freeze({
      resolve: function (id, ok, value) {
        var p = pending.get(id);
        if (!p) return;
        pending.delete(id);
        if (ok) p.resolve(value);
        else p.reject(new Error(String(value)));
      },
      resolveRaw: function (id, value) {
        // Like resolve but keeps null (deep-link:get-stored, context menu).
        var p = pending.get(id);
        if (!p) return;
        pending.delete(id);
        p.resolve(value);
      },
      emit: function (channel, payload) {
        var set = listeners.get(channel);
        if (!set) return;
        set.forEach(function (fn) {
          try { fn(payload); } catch (e) { console.error(e); }
        });
      }
    }),
    writable: false,
    configurable: false
  });

  var zoomFactor = 1;
  Object.defineProperty(window, '__anticraftSetZoomFactor', {
    value: function (f) { zoomFactor = f; },
    writable: false,
    configurable: false
  });

  window.electronUpdater = {
    onStateChanged: function (cb) { return on('updater:state-changed', cb); },
    applyUpdate: function () { return invoke('updater:apply'); },
    quitAndInstall: function () { return invoke('updater:quit-and-install'); },
    checkForUpdates: function () { return invoke('updater:check-for-updates'); },
    getState: function () { return invoke('updater:get-state'); }
  };

  window.dialog = {
    showOpenDialog: function () { return invoke('dialog:open-workspace'); },
    showOpenMultipleFolderDialog: function () { return invoke('dialog:open-workspaces'); }
  };

  window.nativeNotifications = {
    send: function (opts) { return invoke('notification:send', opts); },
    openSystemPreferences: function () { return invoke('notification:open-system-preferences'); },
    onClicked: function (cb) { return on('notification:clicked', cb); }
  };

  window.nativeStorage = {
    getItems: function () { return invoke('storage:get-items'); },
    updateItems: function (changes) { return invoke('storage:update-items', changes); },
    onChanged: function (cb) { return on('storage:changed', cb); }
  };

  window.logs = {
    getElectronLogs: function () { return invoke('logs:electron'); }
  };

  window.extensions = {
    sendAuthorities: function (map) { return invoke('extensions:send-authorities', map); }
  };

  window.deepLink = {
    onDeepLink: function (cb) { return on('deep-link', cb); },
    getStoredDeepLink: function () { return invoke('deep-link:get-stored'); }
  };

  window.agent = {
    updateActiveAgentCount: function (n) { return invoke('agent:update-active-count', n); }
  };

  window.electronNative = {
    // Returns the zoom FACTOR (not level), matching the original.
    getZoomLevel: function () { return zoomFactor; },
    // WebView2 does not expose filesystem paths of DOM Files; best effort.
    getPathForFile: function (file) { return (file && (file.path || file.webkitRelativePath)) || ''; },
    setTitleBarOverlay: function (o) { return invoke('window:set-title-bar-overlay', o); },
    minimize: function () { return invoke('window:minimize'); },
    maximize: function () { return invoke('window:maximize'); },
    unmaximize: function () { return invoke('window:unmaximize'); },
    close: function () { return invoke('window:close'); },
    isMaximized: function () { return invoke('window:is-maximized'); },
    toggleDevTools: function () { return invoke('window:toggle-devtools'); },
    zoomIn: function () { fire('window:zoom-in'); },
    zoomOut: function () { fire('window:zoom-out'); },
    resetZoom: function () { fire('window:reset-zoom'); },
    openExternal: function (url) { return invoke('shell:open-external', url); },
    revealInFilePicker: function (fileUrl) { return invoke('shell:reveal-in-file-picker', fileUrl); },
    showContextMenu: function (items) { return invoke('window:show-context-menu', items); },
    closeContextMenu: function () { return invoke('window:close-context-menu'); },
    executeJavaScriptInIsolatedWorld: function (wcId, worldId, code) {
      return invoke('webview:execute-js-isolated-world', wcId, worldId, code);
    }
  };

  window.ide = {
    isInstalled: function () { return invoke('ide:is-installed'); }
  };

  window.wsl = {
    getState: function () { return invoke('wsl:get-state'); },
    connect: function (distro) { return invoke('wsl:connect', distro); }
  };

  // Per-window keybindings (SPEC §8.3). The webview owns keyboard focus, so
  // they are intercepted here (capture phase, keydown only) and forwarded.
  window.addEventListener('keydown', function (e) {
    if (!e.ctrlKey || e.metaKey) return;
    var k = (e.key || '').toLowerCase();
    if (e.shiftKey && k === 'n') {
      e.preventDefault(); e.stopPropagation();
      fire('__anticraft:new-window');
    } else if (k === 'q') {
      e.preventDefault(); e.stopPropagation();
      fire('__anticraft:quit');
    }
  }, true);

  // Ctrl+wheel zoom (stands in for Electron's zoom-changed). The shell
  // persists it only for windows that own the storage manager.
  window.addEventListener('wheel', function (e) {
    if (!e.ctrlKey) return;
    e.preventDefault();
    fire('__anticraft:wheel-zoom', e.deltaY < 0);
  }, { passive: false, capture: true });
})();
