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
    f.style.cssText = 'border:0;width:380px;max-width:95vw;height:430px;background:transparent;';
    f.setAttribute('sandbox', 'allow-scripts allow-same-origin allow-popups');
    return f;
  }

  if (mode === 'inline') {
    script.parentNode.insertBefore(iframe(), script);
    return;
  }

  var btn = document.createElement('button');
  btn.textContent = '⚡ ' + (script.getAttribute('data-label') || 'Donate');
  btn.style.cssText =
    'position:fixed;bottom:20px;right:20px;z-index:99998;padding:10px 16px;' +
    'border-radius:999px;border:0;background:#f7931a;color:#fff;font:600 15px system-ui;' +
    'cursor:pointer;box-shadow:0 2px 12px rgba(0,0,0,.25);';

  var overlay = null;
  btn.onclick = function () {
    if (overlay) { overlay.remove(); overlay = null; return; }
    overlay = document.createElement('div');
    overlay.style.cssText =
      'position:fixed;inset:0;z-index:99999;background:rgba(0,0,0,.45);' +
      'display:flex;align-items:center;justify-content:center;';
    overlay.onclick = function (e) { if (e.target === overlay) { overlay.remove(); overlay = null; } };
    var f = iframe();
    f.style.borderRadius = '16px';
    overlay.appendChild(f);
    document.body.appendChild(overlay);
  };
  document.body.appendChild(btn);
})();
