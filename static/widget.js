// nutrail donation widget.
// Usage on any site:
//   <script src="https://YOUR-APP.up.railway.app/donate/widget.js" defer></script>
// Optional attributes on the script tag:
//   data-mode="inline"  — render the donate card where the script tag sits (default: floating button)
//   data-label="Donate" — floating button label
(function () {
  var script = document.currentScript;
  var origin = new URL(script.src).origin;
  var mode = script.getAttribute('data-mode') || 'button';

  function iframe() {
    var f = document.createElement('iframe');
    f.src = origin + '/donate';
    f.title = 'Donate sats';
    f.style.cssText = 'border:0;width:400px;max-width:95vw;height:620px;max-height:92vh;background:transparent;color-scheme:normal;';
    f.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-popups');
    f.setAttribute('allow', 'clipboard-write');
    return f;
  }

  if (mode === 'inline') {
    script.parentNode.insertBefore(iframe(), script);
    return;
  }

  // Bespoke bolt mark, bare — no tile behind it.
  var BOLT =
    '<svg width="13" height="15" viewBox="0 0 13 15" fill="none" aria-hidden="true" style="margin-right:7px">' +
    '<path d="M7.8 1 2 8.4h3.4L5.2 14 11 6.6H7.6L7.8 1Z" fill="#1a1206"/></svg>';

  var btn = document.createElement('button');
  btn.type = 'button';
  btn.innerHTML = BOLT + (script.getAttribute('data-label') || 'Donate');
  btn.setAttribute('aria-label', 'Open donation panel');
  btn.style.cssText =
    'position:fixed;bottom:20px;right:20px;z-index:2147483000;display:inline-flex;align-items:center;' +
    'padding:11px 18px;border-radius:10px;border:0;background:#f7a01b;color:#1a1206;' +
    'font:650 14px/1 system-ui,sans-serif;cursor:pointer;' +
    'box-shadow:0 2px 6px rgba(217,134,10,.35);'; // tight, colour-matched — not a black bloom
  btn.onmouseenter = function () { btn.style.background = '#e28d0e'; };
  btn.onmouseleave = function () { btn.style.background = '#f7a01b'; };

  var overlay = null;
  function close() {
    if (overlay) { overlay.remove(); overlay = null; }
    document.removeEventListener('keydown', onKey);
  }
  function onKey(e) { if (e.key === 'Escape') close(); }

  btn.onclick = function () {
    if (overlay) return close();
    overlay = document.createElement('div');
    overlay.style.cssText =
      'position:fixed;inset:0;z-index:2147483001;background:rgba(12,9,7,.55);' +
      'display:flex;align-items:center;justify-content:center;padding:16px;';
    overlay.addEventListener('click', function (e) { if (e.target === overlay) close(); });
    document.addEventListener('keydown', onKey);
    var f = iframe();
    f.style.borderRadius = '14px';
    overlay.appendChild(f);
    document.body.appendChild(overlay);
  };
  document.body.appendChild(btn);
})();
