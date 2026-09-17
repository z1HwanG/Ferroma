/* Ferroma docs site — client behaviour: theme, search, TOC spy, drawer, copy. */
(function () {
  'use strict';

  // ------------------------------------------------------------- theme
  var themeBtn = document.getElementById('themeBtn');
  if (themeBtn) {
    themeBtn.addEventListener('click', function () {
      var next = document.documentElement.dataset.theme === 'light' ? 'dark' : 'light';
      document.documentElement.dataset.theme = next;
      try { localStorage.setItem('ferroma-theme', next); } catch (e) {}
    });
  }

  // ------------------------------------------------------------- mobile drawer
  var menuBtn = document.getElementById('menuBtn');
  var scrim = document.getElementById('scrim');
  function closeNav() { document.body.classList.remove('nav-open'); }
  if (menuBtn) menuBtn.addEventListener('click', function () { document.body.classList.toggle('nav-open'); });
  if (scrim) scrim.addEventListener('click', closeNav);

  // ------------------------------------------------------------- copy buttons
  document.querySelectorAll('[data-copy]').forEach(function (btn) {
    btn.addEventListener('click', function () {
      var pre = btn.parentElement && btn.parentElement.querySelector('pre');
      if (!pre) return;
      var text = pre.textContent || '';
      var done = function () {
        btn.textContent = '✓'; btn.classList.add('ok');
        setTimeout(function () { btn.textContent = '⧉'; btn.classList.remove('ok'); }, 1400);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, done);
      } else {
        var ta = document.createElement('textarea');
        ta.value = text; document.body.appendChild(ta); ta.select();
        try { document.execCommand('copy'); } catch (e) {}
        document.body.removeChild(ta); done();
      }
    });
  });

  // ------------------------------------------------------------- toc scroll spy
  var tocLinks = document.querySelectorAll('.toc a');
  if (tocLinks.length && 'IntersectionObserver' in window) {
    var byId = {};
    tocLinks.forEach(function (a) { byId[a.getAttribute('href').slice(1)] = a; });
    var visible = {};
    var pick = function () {
      var best = null;
      for (var id in visible) {
        if (!visible[id]) continue;
        if (!best || document.getElementById(id).getBoundingClientRect().top < document.getElementById(best).getBoundingClientRect().top) best = id;
      }
      tocLinks.forEach(function (a) { a.classList.remove('here'); });
      if (best && byId[best]) byId[best].classList.add('here');
    };
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (en) { visible[en.target.id] = en.isIntersecting; });
      pick();
    }, { rootMargin: '-70px 0px -66% 0px', threshold: 0 });
    Object.keys(byId).forEach(function (id) {
      var el = document.getElementById(id);
      if (el) io.observe(el);
    });
  }

  // ------------------------------------------------------------- search
  var input = document.getElementById('searchInput');
  var box = document.getElementById('searchResults');
  if (!input || !box) return;
  var INDEX = (window.FERROMA_SEARCH || {})[document.documentElement.lang.startsWith('zh') ? 'zh' : 'en'] || [];
  var IS_ZH = document.documentElement.lang.toLowerCase().startsWith('zh');
  var NO_RESULTS = IS_ZH ? '没有匹配结果' : 'No results';
  var sel = -1, items = [];

  var esc = function (s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); };
  var rxCache = {};
  function mark(text, q) {
    var i = text.toLowerCase().indexOf(q.toLowerCase());
    if (i < 0) return esc(text);
    return esc(text.slice(0, i)) + '<mark>' + esc(text.slice(i, i + q.length)) + '</mark>' + esc(text.slice(i + q.length));
  }

  function search(q) {
    var ql = q.toLowerCase();
    var out = [];
    for (var pi = 0; pi < INDEX.length && out.length < 8; pi++) {
      var page = INDEX[pi];
      var titleHit = page.t.toLowerCase().indexOf(ql);
      for (var si = 0; si < page.s.length && out.length < 8; si++) {
        var seg = page.s[si];
        var inHead = seg.h.toLowerCase().indexOf(ql);
        var at = seg.x.toLowerCase().indexOf(ql);
        var score = -1;
        if (inHead === 0) score = 100;
        else if (inHead >= 0) score = 60;
        else if (at >= 0) score = 30;
        else if (titleHit >= 0 && seg.id === '') score = 20;
        if (score < 0) continue;
        if (titleHit >= 0) score += 10;
        var snippet;
        if (inHead >= 0) {
          snippet = seg.x.slice(0, 110);
          out.push({ p: page, s: seg, score: score, head: seg.h, snippet: snippet });
        } else {
          var start = Math.max(0, at - 34);
          snippet = (start > 0 ? '…' : '') + seg.x.slice(start, start + 118);
          out.push({ p: page, s: seg, score: score, head: seg.h === page.t ? null : seg.h, snippet: snippet, at: at - start });
        }
      }
    }
    out.sort(function (a, b) { return b.score - a.score; });
    return out.slice(0, 8);
  }

  function render(q) {
    if (!q) { close(); return; }
    var res = search(q);
    items = res;
    if (!res.length) {
      box.innerHTML = '<div class="sr-empty">' + NO_RESULTS + '</div>';
      box.hidden = false; sel = -1;
      return;
    }
    box.innerHTML = res.map(function (r, i) {
      var href = (document.body.classList.contains('home') ? '' : '../') + r.p.u + (r.s.id ? '#' + r.s.id : '');
      var headHtml = r.head ? '<span class="sr-head">' + mark(r.head, q) + '</span>' : '';
      var snip = r.snippet;
      var snipHtml = snip.toLowerCase().indexOf(q.toLowerCase()) >= 0 ? mark(snip, q) : esc(snip);
      return '<a class="sr-item" data-i="' + i + '" href="' + href + '"><div class="sr-title">' + mark(r.p.t, q) + headHtml + '</div><div class="sr-snippet">' + snipHtml + '</div></a>';
    }).join('');
    box.hidden = false; sel = -1;
  }

  function close() { box.hidden = true; sel = -1; items = []; }
  function move(d) {
    if (!items.length) return;
    sel = (sel + d + items.length) % items.length;
    var els = box.querySelectorAll('.sr-item');
    els.forEach(function (el, i) { el.classList.toggle('sel', i === sel); });
    els[sel].scrollIntoView({ block: 'nearest' });
  }

  input.addEventListener('input', function () { render(input.value.trim()); });
  input.addEventListener('focus', function () { if (input.value.trim()) render(input.value.trim()); });
  input.addEventListener('keydown', function (e) {
    if (e.key === 'ArrowDown') { e.preventDefault(); move(1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); move(-1); }
    else if (e.key === 'Enter') {
      var target = sel >= 0 ? items[sel] : items[0];
      if (target) { location.href = (document.body.classList.contains('home') ? '' : '../') + target.p.u + (target.s.id ? '#' + target.s.id : ''); close(); }
    } else if (e.key === 'Escape') { close(); input.blur(); }
  });
  document.addEventListener('click', function (e) { if (!e.target.closest('.search')) close(); });

  // "/" focuses search
  document.addEventListener('keydown', function (e) {
    if (e.key === '/' && !e.ctrlKey && !e.metaKey && !e.altKey) {
      var tag = (document.activeElement && document.activeElement.tagName) || '';
      if (tag !== 'INPUT' && tag !== 'TEXTAREA') { e.preventDefault(); input.focus(); }
    }
  });

  // close drawer after navigating via sidebar on mobile
  document.querySelectorAll('.sidebar .nav-link').forEach(function (a) {
    a.addEventListener('click', closeNav);
  });
})();