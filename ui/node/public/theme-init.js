// Runs before first paint (external file: the console CSP forbids inline scripts).
(function () {
  var t = null;
  try { t = localStorage.getItem('fluxvm-theme'); } catch (e) { /* storage blocked */ }
  if (t !== 'dark' && t !== 'light') {
    t = window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  }
  document.documentElement.setAttribute('data-theme', t);
  var m = document.querySelector('meta[name="theme-color"]');
  if (m) m.setAttribute('content', t === 'dark' ? '#000000' : '#ffffff');
})();
