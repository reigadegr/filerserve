(function () {
  var breadcrumb = document.getElementById('breadcrumb');
  var fileList = document.getElementById('file-list');
  var loading = document.getElementById('loading');
  var errorDiv = document.getElementById('error');

  function currentPath() {
    return location.hash.slice(1);
  }

  function navigate(path) {
    location.hash = path;
  }

  function joinPath(prefix, name) {
    var enc = encodeURIComponent(name);
    return prefix ? prefix + '/' + enc : enc;
  }

  function loadListing(path) {
    loading.style.display = 'block';
    errorDiv.style.display = 'none';
    fileList.innerHTML = '';

    var url = path ? '/api/list/' + path : '/api/list';
    fetch(url)
      .then(function (res) {
        if (!res.ok) throw new Error('HTTP ' + res.status);
        return res.json();
      })
      .then(function (data) {
        render(data, path);
      })
      .catch(function (err) {
        loading.style.display = 'none';
        errorDiv.textContent = '加载失败: ' + err.message;
        errorDiv.style.display = 'block';
      });
  }

  function render(data, encodedPath) {
    loading.style.display = 'none';

    renderBreadcrumb(data.path);

    var entries = data.entries;
    if (entries.length === 0) {
      var row = fileList.insertRow();
      row.className = 'empty-row';
      var cell = row.insertCell();
      cell.colSpan = 4;
      cell.textContent = '空目录';
      return;
    }

    var port = data.port;
    var lanIp = data.lan_ip;
    var prefix = encodedPath;

    entries.forEach(function (entry) {
      var row = fileList.insertRow();

      var nameCell = row.insertCell();
      nameCell.className = 'name-cell ' + entry.type;
      var icon = document.createElement('span');
      icon.className = entry.type === 'dir' ? 'dir-icon' : 'file-icon';
      nameCell.appendChild(icon);
      nameCell.appendChild(document.createTextNode(' ' + entry.name));

      var fp = joinPath(prefix, entry.name);

      if (entry.type === 'dir') {
        (function (p) {
          nameCell.onclick = function () { navigate(p); };
        })(fp);
      }

      var sizeCell = row.insertCell();
      sizeCell.className = 'size-col';
      sizeCell.textContent = entry.size != null ? humanSize(entry.size) : '—';

      var modCell = row.insertCell();
      modCell.className = 'modified-col';
      modCell.textContent = entry.modified ? entry.modified.replace('T', ' ') : '—';

      var linkCell = row.insertCell();
      linkCell.className = 'link-group';

      if (entry.type === 'file') {
        var base = '/files/' + fp;
        if (lanIp) {
          linkCell.appendChild(makeCopyBtn('局域网', 'http://' + lanIp + ':' + port + base));
        }
        linkCell.appendChild(makeCopyBtn('本地', 'http://127.0.0.1:' + port + base));
      } else {
        linkCell.textContent = '—';
        linkCell.style.color = '#999';
      }
    });
  }

  function renderBreadcrumb(decodedPath) {
    breadcrumb.innerHTML = '';
    var home = document.createElement('a');
    home.textContent = '🏠 首页';
    home.onclick = function () { navigate(''); };
    breadcrumb.appendChild(home);

    if (decodedPath && decodedPath !== '/') {
      var parts = decodedPath.slice(1).split('/');
      var acc = '';
      parts.forEach(function (part, i) {
        acc += (i > 0 ? '/' : '') + part;
        var sep = document.createElement('span');
        sep.className = 'sep';
        sep.textContent = '/';
        breadcrumb.appendChild(sep);
        var link = document.createElement('a');
        link.textContent = part;
        (function (p) {
          link.onclick = function () { navigate(p); };
        })(acc);
        breadcrumb.appendChild(link);
      });
    }
  }

  function makeCopyBtn(label, text) {
    var btn = document.createElement('button');
    btn.className = 'copy-btn';
    btn.textContent = label;
    btn.onclick = function () { copyText(text, btn); };
    return btn;
  }

  function humanSize(bytes) {
    var units = ['B', 'KB', 'MB', 'GB', 'TB'];
    var i = 0;
    while (bytes >= 1024 && i < units.length - 1) {
      bytes /= 1024;
      i++;
    }
    return (i === 0 ? Math.round(bytes) : bytes.toFixed(1)) + ' ' + units[i];
  }

  function copyText(text, btn) {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(function () {
        showCopied(btn);
      });
    } else {
      var ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      try { document.execCommand('copy'); showCopied(btn); } catch (e) {}
      document.body.removeChild(ta);
    }
  }

  function showCopied(btn) {
    var original = btn.textContent;
    btn.classList.add('copied');
    btn.textContent = '已复制';
    setTimeout(function () {
      btn.classList.remove('copied');
      btn.textContent = original;
    }, 1500);
  }

  window.addEventListener('hashchange', function () {
    loadListing(currentPath());
  });

  loadListing(currentPath());
})();
