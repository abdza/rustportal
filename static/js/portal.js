// Tab key inserts spaces in code editors
document.addEventListener('keydown', function (e) {
  var t = e.target;
  if (e.key === 'Tab' && t && t.classList && t.classList.contains('code-edit')) {
    e.preventDefault();
    var s = t.selectionStart;
    t.value = t.value.substring(0, s) + '    ' + t.value.substring(t.selectionEnd);
    t.selectionStart = t.selectionEnd = s + 4;
  }
});
