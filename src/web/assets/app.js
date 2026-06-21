// Browser client for the herdr TUI.
//
// The server sends already-diffed terminal escape bytes, so xterm.js is a pure
// display here: every key is intercepted and forwarded as a structured event
// instead of letting xterm generate its own bytes.

(function () {
  "use strict";

  var RECONNECT_MIN_MS = 500;
  var RECONNECT_MAX_MS = 10000;

  // crossterm KeyModifiers bits, matching src/protocol/wire.rs.
  var MOD_SHIFT = 0b0001;
  var MOD_CONTROL = 0b0010;
  var MOD_ALT = 0b0100;
  var MOD_SUPER = 0b1000;

  // KeyboardEvent.key values that map to a named protocol key. Anything else of
  // length one is sent as a character; anything else is dropped.
  var NAMED_KEYS = {
    Enter: "enter",
    Backspace: "backspace",
    Tab: "tab",
    Escape: "escape",
    Delete: "delete",
    Insert: "insert",
    Home: "home",
    End: "end",
    PageUp: "pageup",
    PageDown: "pagedown",
    ArrowLeft: "left",
    ArrowRight: "right",
    ArrowUp: "up",
    ArrowDown: "down",
  };

  var term = new Terminal({
    allowProposedApi: true,
    cursorBlink: true,
    convertEol: false,
    fontFamily:
      'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
    fontSize: 14,
    scrollback: 0,
    theme: { background: "#101014" },
  });
  var fitAddon = new FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(document.getElementById("terminal"));

  var statusEl = document.getElementById("status");
  var statusTextEl = document.getElementById("status-text");
  var statusHintEl = document.getElementById("status-hint");

  var socket = null;
  var reconnectDelay = RECONNECT_MIN_MS;
  var reconnectTimer = null;
  var giveUp = false;
  var mouseCapture = false;
  var reportKeyReleases = false;
  var lastSize = { cols: 0, rows: 0 };

  function showStatus(state, text, hint) {
    statusEl.hidden = false;
    statusEl.dataset.state = state;
    statusTextEl.textContent = text;
    statusHintEl.hidden = !hint;
    statusHintEl.textContent = hint || "";
  }

  function hideStatus() {
    statusEl.hidden = true;
    delete statusEl.dataset.state;
  }

  // A transient toast must never hide a connection state that replaced it in
  // the meantime, so it only clears itself if it is still the one showing.
  function showNotice(text) {
    showStatus("notice", text);
    setTimeout(function () {
      if (statusEl.dataset.state === "notice") {
        hideStatus();
      }
    }, 4000);
  }

  // A session that will never be accepted again must stop retrying, or the page
  // spins forever against a server that has already forgotten it.
  function stopForGood(reason) {
    giveUp = true;
    if (reconnectTimer !== null) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    showStatus("closed", reason, "herdr web connect");
  }

  function send(message) {
    if (socket && socket.readyState === WebSocket.OPEN) {
      socket.send(JSON.stringify(message));
    }
  }

  function sendInput(events) {
    if (events.length > 0) {
      send({ type: "input", events: events });
    }
  }

  function modifiersOf(event) {
    var modifiers = 0;
    if (event.shiftKey) modifiers |= MOD_SHIFT;
    if (event.ctrlKey) modifiers |= MOD_CONTROL;
    if (event.altKey) modifiers |= MOD_ALT;
    if (event.metaKey) modifiers |= MOD_SUPER;
    return modifiers;
  }

  function keyNameOf(event) {
    if (Object.prototype.hasOwnProperty.call(NAMED_KEYS, event.key)) {
      return NAMED_KEYS[event.key];
    }
    if (/^F([1-9]|1[0-9]|2[0-4])$/.test(event.key)) {
      return event.key.toLowerCase();
    }
    // `key` is a single grapheme for printable input, including while Ctrl or
    // Alt are held, which is exactly what the protocol wants.
    if (Array.from(event.key).length === 1) {
      return event.key;
    }
    return null;
  }

  function connect() {
    reconnectTimer = null;
    var protocol = location.protocol === "https:" ? "wss:" : "ws:";
    var size = fitAddon.proposeDimensions() || { cols: term.cols, rows: term.rows };
    var url =
      protocol +
      "//" +
      location.host +
      "/ws?cols=" +
      encodeURIComponent(size.cols) +
      "&rows=" +
      encodeURIComponent(size.rows);

    socket = new WebSocket(url);
    socket.binaryType = "arraybuffer";

    socket.onopen = function () {
      reconnectDelay = RECONNECT_MIN_MS;
      hideStatus();
      // Force a resize report so the server's idea of the viewport matches the
      // one the fit addon just measured.
      lastSize = { cols: 0, rows: 0 };
      resize();
      term.focus();
    };

    socket.onmessage = function (event) {
      if (typeof event.data === "string") {
        handleControl(JSON.parse(event.data));
      } else {
        term.write(new Uint8Array(event.data));
      }
    };

    socket.onclose = function () {
      socket = null;
      if (giveUp) {
        return;
      }
      scheduleReconnect();
    };

    socket.onerror = function () {
      // `onclose` always follows, and it owns the retry decision.
    };
  }

  // A closed socket is ambiguous: the server may be restarting, or this session
  // may be gone. Probing the page tells the two apart — a 401 means the cookie
  // is dead and no amount of retrying will fix it.
  function scheduleReconnect() {
    showStatus("reconnecting", "Reconnecting…");
    reconnectTimer = setTimeout(function () {
      fetch("/", { method: "HEAD", cache: "no-store" })
        .then(function (response) {
          if (response.status === 401) {
            stopForGood("This session has ended.");
            return;
          }
          connect();
        })
        .catch(function () {
          connect();
        });
      reconnectDelay = Math.min(reconnectDelay * 2, RECONNECT_MAX_MS);
    }, reconnectDelay);
  }

  function handleControl(message) {
    switch (message.type) {
      case "clipboard":
        writeClipboard(message.data);
        break;
      case "title":
        document.title = message.title || "herdr";
        break;
      case "notify":
        showNotice(
          message.body ? message.message + " — " + message.body : message.message
        );
        break;
      case "mouse_capture":
        setMouseCapture(message.enabled);
        break;
      case "report_key_releases":
        reportKeyReleases = message.enabled;
        break;
      case "closed":
        stopForGood(message.reason);
        break;
    }
  }

  // Mouse tracking modes: X11 reporting, button-event (drag) tracking, and SGR
  // coordinates — the same set the native client turns on in a host terminal.
  var MOUSE_TRACKING_ON = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
  var MOUSE_TRACKING_OFF = "\x1b[?1000l\x1b[?1002l\x1b[?1006l";

  // Herdr runs its own selection, paints its own highlight, and copies through
  // OSC 52. A terminal stops selecting on its own once the application asks for
  // mouse events, which is how the native client suppresses it — but the server
  // reports mouse capture as a control message rather than in the render
  // stream, so xterm has to be told separately. Without this it keeps selecting
  // across the whole grid, sidebar included.
  //
  // xterm's own mouse reports go out through `onData`, which nothing subscribes
  // to; input is forwarded as structured events instead.
  function setMouseCapture(enabled) {
    mouseCapture = enabled;
    term.write(enabled ? MOUSE_TRACKING_ON : MOUSE_TRACKING_OFF);
    if (enabled) {
      clearBrowserSelection();
    }
  }

  function clearBrowserSelection() {
    term.clearSelection();
    var selection = window.getSelection();
    if (selection) {
      selection.removeAllRanges();
    }
  }

  function writeClipboard(base64) {
    if (!navigator.clipboard || !navigator.clipboard.writeText) {
      return;
    }
    var binary = atob(base64);
    var bytes = new Uint8Array(binary.length);
    for (var i = 0; i < binary.length; i += 1) {
      bytes[i] = binary.charCodeAt(i);
    }
    navigator.clipboard.writeText(new TextDecoder().decode(bytes)).catch(function () {
      // Firefox wants a user gesture for programmatic writes, and an insecure
      // origin has no clipboard at all. Losing a copy is not worth a dialog.
    });
  }

  function resize() {
    fitAddon.fit();
    if (term.cols === lastSize.cols && term.rows === lastSize.rows) {
      return;
    }
    lastSize = { cols: term.cols, rows: term.rows };
    send({ type: "resize", cols: term.cols, rows: term.rows });
  }

  // xterm.js never generates input of its own — returning false suppresses its
  // handling so every key travels as a structured event instead.
  term.attachCustomKeyEventHandler(function (event) {
    // While an IME is composing, `key` reports the in-progress character and
    // sending it would duplicate what compositionend commits.
    if (event.isComposing || event.keyCode === 229) {
      return false;
    }
    if (event.type === "keydown") {
      var name = keyNameOf(event);
      if (name !== null) {
        event.preventDefault();
        sendInput([
          {
            kind: "key",
            key: name,
            modifiers: modifiersOf(event),
            press: event.repeat ? "repeat" : "down",
          },
        ]);
      }
    } else if (event.type === "keyup" && reportKeyReleases) {
      var released = keyNameOf(event);
      if (released !== null) {
        sendInput([
          { kind: "key", key: released, modifiers: modifiersOf(event), press: "up" },
        ]);
      }
    }
    return false;
  });

  var textarea = term.textarea;
  if (textarea) {
    // Composed input (IME, dictation) commits here rather than as keydowns.
    textarea.addEventListener("compositionend", function (event) {
      if (event.data) {
        sendInput([{ kind: "text", text: event.data }]);
      }
    });
  }

  document.addEventListener("paste", function (event) {
    if (!event.clipboardData) {
      return;
    }
    event.preventDefault();

    var items = event.clipboardData.items || [];
    for (var i = 0; i < items.length; i += 1) {
      if (items[i].type.indexOf("image/") === 0) {
        sendClipboardImage(items[i].getAsFile(), items[i].type);
        return;
      }
    }

    var text = event.clipboardData.getData("text");
    if (text) {
      sendInput([{ kind: "paste", text: text }]);
    }
  });

  function sendClipboardImage(file, mime) {
    if (!file) {
      return;
    }
    var reader = new FileReader();
    reader.onload = function () {
      var bytes = new Uint8Array(reader.result);
      var binary = "";
      for (var i = 0; i < bytes.length; i += 1) {
        binary += String.fromCharCode(bytes[i]);
      }
      send({
        type: "clipboard_image",
        extension: mime.slice("image/".length),
        data: btoa(binary),
      });
    };
    reader.readAsArrayBuffer(file);
  }

  function cellAt(event) {
    var rect = term.element.getBoundingClientRect();
    if (rect.width === 0 || rect.height === 0) {
      return null;
    }
    var column = Math.floor(((event.clientX - rect.left) / rect.width) * term.cols);
    var row = Math.floor(((event.clientY - rect.top) / rect.height) * term.rows);
    return {
      column: Math.max(0, Math.min(term.cols - 1, column)),
      row: Math.max(0, Math.min(term.rows - 1, row)),
    };
  }

  var BUTTONS = { 0: "left", 1: "middle", 2: "right" };
  var dragging = null;

  function sendMouse(action, event, button) {
    var cell = cellAt(event);
    if (cell === null) {
      return;
    }
    sendInput([
      {
        kind: "mouse",
        action: action,
        button: button || "left",
        column: cell.column,
        row: cell.row,
        modifiers: modifiersOf(event),
      },
    ]);
  }

  term.element.addEventListener("mousedown", function (event) {
    if (!mouseCapture) {
      return;
    }
    event.preventDefault();
    dragging = BUTTONS[event.button] || "left";
    sendMouse("down", event, dragging);
  });

  term.element.addEventListener("mousemove", function (event) {
    if (!mouseCapture) {
      return;
    }
    sendMouse(dragging ? "drag" : "move", event, dragging);
  });

  window.addEventListener("mouseup", function (event) {
    if (!mouseCapture || !dragging) {
      return;
    }
    sendMouse("up", event, dragging);
    dragging = null;
    // xterm still tracks a selection under the hidden overlay; drop it so a
    // browser copy shortcut cannot return something the user never saw.
    clearBrowserSelection();
  });

  term.element.addEventListener("contextmenu", function (event) {
    if (mouseCapture) {
      event.preventDefault();
    }
  });

  term.element.addEventListener(
    "wheel",
    function (event) {
      if (!mouseCapture) {
        return;
      }
      event.preventDefault();
      var action = event.deltaY !== 0
        ? event.deltaY < 0
          ? "scroll_up"
          : "scroll_down"
        : event.deltaX < 0
          ? "scroll_left"
          : "scroll_right";
      sendMouse(action, event);
    },
    { passive: false }
  );

  window.addEventListener("focus", function () {
    sendInput([{ kind: "focus", gained: true }]);
  });

  window.addEventListener("blur", function () {
    sendInput([{ kind: "focus", gained: false }]);
  });

  window.addEventListener("resize", resize);
  if (window.visualViewport) {
    window.visualViewport.addEventListener("resize", resize);
  }

  fitAddon.fit();
  connect();
})();
