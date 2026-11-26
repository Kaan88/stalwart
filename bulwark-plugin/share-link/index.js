// Public links for Bulwark's Files: "Get public link" in a file's menu opens
// a dialog that makes, copies and revokes links through the Stalwart share
// link API (POST /api/sharelink/{list,create,revoke}). See README.md.
'use strict';

var React = require('react');
var api = require('@plugin-host');
var h = React.createElement;

var API = '/api/sharelink/';
var EXPIRY_CHOICES = [
  { value: '86400', label: '1 day' },
  { value: '604800', label: '7 days' },
  { value: '2592000', label: '30 days' },
  { value: '31536000', label: '1 year' },
];

/** Calls the server; resolves to its JSON answer or throws with its words. */
async function call(action, body) {
  var res = await api.http.post(API + action, body);
  if (!res || !res.ok) {
    var data = (res && res.data) || {};
    var status = res ? res.status : 0;
    if (status === 404 && action === 'list') {
      throw new Error('This server has no share links (404). See the plugin README.');
    }
    throw new Error(data.detail || data.title || ('The server answered ' + status + '.'));
  }
  return res.data;
}

/** The file the request is about, with what lets the server check it is the right one. */
function fileRef(file) {
  return { fileId: file.id, expectName: file.name, expectSize: file.size };
}

function formatDate(seconds) {
  try {
    return new Date(seconds * 1000).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
  } catch (e) {
    return new Date(seconds * 1000).toISOString();
  }
}

async function copyText(text, input) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) {
    // Clipboard refused: select the text so the user can copy it.
    if (input) {
      input.focus();
      input.select();
    }
    return false;
  }
}

exports.activate = async function () {
  await api.ui.setActions('file-actions', [
    { id: 'share-link', label: 'Get public link', icon: 'link', order: 50 },
  ]);
};

exports.hooks = {
  async onAction(event) {
    if (event.surface !== 'file-actions' || event.actionId !== 'share-link') return;
    var files = (event.context && event.context.files) || [];
    if (files.length !== 1) {
      api.toast.info('Choose one file to get a link for.');
      return;
    }
    var file = files[0];
    if (file.type !== 'file') {
      api.toast.info('Only files can have a public link, not folders.');
      return;
    }
    await api.ui.openDialog({
      title: 'Public link: ' + file.name,
      slot: 'plugin-dialog',
      extraProps: { file: file },
      width: 560,
    });
  },
};

function LinkRow(props) {
  var ui = require('@plugin-host/ui');
  var link = props.link;
  var inputRef = React.useRef(null);
  return h(ui.Stack, { gap: 1 },
    h(ui.Row, { gap: 2, wrap: false },
      h(ui.Input, {
        ref: inputRef,
        readOnly: true,
        value: link.url,
        'aria-label': 'Public link',
        onFocus: function (e) { e.target.select(); },
        style: { flex: 1, minWidth: 0, fontFamily: 'monospace' },
      }),
      h(ui.Button, {
        variant: 'secondary',
        size: 'sm',
        onClick: function () {
          copyText(link.url, inputRef.current).then(function (copied) {
            if (copied) api.toast.success('Link copied.');
            else api.toast.info('Copy the selected link with Ctrl+C.');
          });
        },
      }, 'Copy'),
      h(ui.Button, {
        variant: 'destructive',
        size: 'sm',
        busy: props.busy,
        onClick: function () { props.onRevoke(link); },
      }, 'Revoke'),
    ),
    h(ui.Text, { tone: 'muted', size: 'xs' }, 'Expires ' + formatDate(link.expires)),
  );
}

function ShareDialog(props) {
  var ui = require('@plugin-host/ui');
  var file = props.file;
  var _a = React.useState(null), links = _a[0], setLinks = _a[1];
  var _b = React.useState(null), error = _b[0], setError = _b[1];
  var _c = React.useState('604800'), expiry = _c[0], setExpiry = _c[1];
  var _d = React.useState(null), busy = _d[0], setBusy = _d[1];

  React.useEffect(function () {
    var live = true;
    call('list', fileRef(file))
      .then(function (list) { if (live) setLinks(list); })
      .catch(function (e) { if (live) { setLinks([]); setError(e.message); } });
    return function () { live = false; };
  }, [file.id]);

  function create() {
    setBusy('create');
    setError(null);
    var body = fileRef(file);
    body.expiresIn = Number(expiry);
    call('create', body)
      .then(function (link) {
        setLinks(function (list) { return [link].concat(list || []); });
        return copyText(link.url, null).then(function (copied) {
          api.toast.success(copied ? 'Link made and copied.' : 'Link made.');
        });
      })
      .catch(function (e) { setError(e.message); })
      .finally(function () { setBusy(null); });
  }

  function revoke(link) {
    setBusy(link.id);
    setError(null);
    var body = fileRef(file);
    body.linkId = link.id;
    call('revoke', body)
      .then(function () {
        setLinks(function (list) { return (list || []).filter(function (l) { return l.id !== link.id; }); });
        api.toast.success('Link revoked. It no longer works.');
      })
      .catch(function (e) { setError(e.message); })
      .finally(function () { setBusy(null); });
  }

  var body;
  if (links === null) {
    body = h(ui.Loading, { label: 'Loading links' });
  } else if (links.length === 0) {
    body = h(ui.Text, { tone: 'muted' }, 'No public link yet. Anyone you give a link to can download this file without signing in, until the link expires.');
  } else {
    body = h(ui.Stack, { gap: 3 }, links.map(function (link) {
      return h(LinkRow, { key: link.id, link: link, busy: busy === link.id, onRevoke: revoke });
    }));
  }

  return h(ui.Stack, { gap: 4 },
    error ? h(ui.Alert, { tone: 'danger' }, error) : null,
    body,
    h(ui.Divider, null),
    h(ui.Row, { gap: 2, wrap: true },
      h(ui.Text, null, 'New link, valid for'),
      h(ui.Select, {
        value: expiry,
        options: EXPIRY_CHOICES,
        onChange: function (value) { setExpiry(value); },
        'aria-label': 'Link valid for',
      }),
      h(ui.Button, { icon: 'link', busy: busy === 'create', onClick: create }, 'Make link'),
    ),
    h(ui.DialogButtons, {
      buttons: [{ id: 'close', label: 'Done', variant: 'secondary' }],
      onClick: function () { if (props.onResult) props.onResult(true); },
    }),
  );
}

exports.slots = {
  'plugin-dialog': { component: ShareDialog },
};
