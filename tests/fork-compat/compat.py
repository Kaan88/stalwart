#!/usr/bin/env python3
"""Seed and verify data for tests/fork-compat/run.sh.

  compat.py seed   <server dir> <state.json>
      Uploads the test data on a running server (the fork) and writes what
      was uploaded, with SHA-256 sums, JMAP states and quotas, to state.json.
  compat.py verify <server dir> <state.json> <mode>
      Checks a running server against state.json. mode is "all" (every file,
      mail, event and contact as seeded) or "nuke" (big files gone, the rest
      intact, quotas and change logs as expected).
  compat.py roundtrip <server dir>
      Uploads, replaces and deletes a file over WebDAV and checks each step.

Prints PASS/FAIL lines and exits non-zero on any failure.
"""

import base64
import hashlib
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request

URL = os.environ.get("URL", "http://127.0.0.1:18080")
MIB = 1024 * 1024
CHUNK = 8 * MIB
FAILURES = 0


def ok(msg):
    print(f"PASS  {msg}")


def bad(msg):
    global FAILURES
    FAILURES += 1
    print(f"FAIL  {msg}")


def check(msg, got, want):
    if got == want:
        ok(msg)
    else:
        bad(f"{msg} (got {got!r}, want {want!r})")


class Client:
    def __init__(self, server_dir, user):
        with open(os.path.join(server_dir, "etc", f"{user}.txt")) as f:
            self.login, self.password = f.read().strip().split(":", 1)
        token = base64.b64encode(f"{self.login}:{self.password}".encode()).decode()
        self.auth = f"Basic {token}"
        self.dav_home = f"/dav/file/{urllib.parse.quote(self.login)}/"
        self.account = None

    def request(self, method, path, body=None, headers=None, auth=True):
        req = urllib.request.Request(URL + path, data=body, method=method)
        if auth:
            req.add_header("Authorization", self.auth)
        for k, v in (headers or {}).items():
            req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=600) as resp:
                return resp.status, resp.read(), dict(resp.headers)
        except urllib.error.HTTPError as err:
            return err.code, err.read(), dict(err.headers)

    def jmap(self, using, calls):
        body = json.dumps({"using": ["urn:ietf:params:jmap:core"] + using, "methodCalls": calls}).encode()
        status, data, _ = self.request("POST", "/jmap/", body, {"Content-Type": "application/json"})
        if status != 200:
            raise RuntimeError(f"JMAP {status}: {data[:300]!r}")
        return json.loads(data)["methodResponses"]

    def session(self):
        status, data, _ = self.request("GET", "/jmap/session")
        session = json.loads(data)
        self.account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
        return session

    def upload(self, data, ctype="application/octet-stream"):
        status, body, _ = self.request("POST", f"/jmap/upload/{self.account}/", data, {"Content-Type": ctype})
        if status != 200:
            raise RuntimeError(f"upload {status}: {body[:300]!r}")
        return json.loads(body)

    def dav_path(self, name):
        return self.dav_home + urllib.parse.quote(name)


FILES = "urn:ietf:params:jmap:filenode"
MAIL = "urn:ietf:params:jmap:mail"


def content(seed, size):
    """Deterministic pseudo-random bytes (so a rerun gives the same sums)."""
    return hashlib.shake_256(seed.encode()).digest(size) if size else b""


def sha(data):
    return hashlib.sha256(data).hexdigest()


def file_nodes(client):
    resp = client.jmap([FILES], [
        ["FileNode/query", {"accountId": client.account}, "0"],
        ["FileNode/get", {"accountId": client.account, "#ids": {"resultOf": "0", "name": "FileNode/query", "path": "/ids"},
                          "properties": ["name", "size", "blobId", "parentId"]}, "1"],
    ])
    return {n["name"]: n for n in resp[1][1]["list"] if n.get("@type") == "File"}, resp[1][1]["state"]


def quota_used(client):
    resp = client.jmap(["urn:ietf:params:jmap:quota"], [["Quota/get", {"accountId": client.account}, "0"]])
    return {q.get("name", q["id"]) + ":" + ",".join(sorted(q.get("types", []))): q.get("used") for q in resp[0][1]["list"]}


def mail_state(client):
    resp = client.jmap([MAIL], [
        ["Email/query", {"accountId": client.account}, "0"],
        ["Email/get", {"accountId": client.account, "#ids": {"resultOf": "0", "name": "Email/query", "path": "/ids"},
                       "properties": ["subject", "blobId", "size"]}, "1"],
    ])
    return {e["subject"]: e for e in resp[1][1]["list"]}, resp[1][1]["state"]


def eml(subject, attachment):
    boundary = "b0undary"
    lines = [
        "From: tester@kweb.test", "To: tester@kweb.test", f"Subject: {subject}",
        "Date: Mon, 16 Nov 2026 10:00:00 +0000", f"Message-ID: <{sha(subject.encode())[:16]}@kweb.test>",
        "MIME-Version: 1.0",
    ]
    if attachment is None:
        lines += ["Content-Type: text/plain; charset=utf-8", "", "Hello, a normal mail."]
        return ("\r\n".join(lines) + "\r\n").encode()
    lines += [f'Content-Type: multipart/mixed; boundary="{boundary}"', "", f"--{boundary}",
              "Content-Type: text/plain", "", "See the attachment.", f"--{boundary}",
              "Content-Type: application/octet-stream", 'Content-Disposition: attachment; filename="big.bin"',
              "Content-Transfer-Encoding: base64", ""]
    b64 = base64.encodebytes(attachment).decode().replace("\n", "\r\n")
    return ("\r\n".join(lines) + "\r\n" + b64 + f"\r\n--{boundary}--\r\n").encode()


def seed(server_dir, state_path):
    t = Client(server_dir, "tester")
    u = Client(server_dir, "second")
    t.session(); u.session()
    state = {"files": [], "uploads": [], "mail": [], "links": [], "accounts": {}}

    def put(client, owner, name, data, big):
        status, body, _ = client.request("PUT", client.dav_path(name), data, {"Content-Type": "application/octet-stream"})
        check(f"seed PUT {name} ({len(data)} bytes)", status, 201)
        state["files"].append({"owner": owner, "name": name, "size": len(data), "sha": sha(data), "big": big})

    # WebDAV uploads (big = stored chunked: more than 8 MiB).
    put(t, "tester", "tiny.txt", content("tiny", 4096), False)
    put(t, "tester", "exact8.bin", content("exact8", CHUNK), False)
    put(t, "tester", "eight1.bin", content("eight1", CHUNK + 1), True)
    put(t, "tester", "twenty.bin", content("twenty", 20 * MIB), True)
    put(t, "tester", "hundred.bin", content("hundred", 100 * MIB + 7), True)
    put(t, "tester", "empty.bin", b"", False)
    put(t, "tester", "résumé ünïcode 文件.txt", content("uni-small", 5000), False)
    put(t, "tester", "grand fichier é 大.bin", content("uni-big", 9 * MIB), True)
    put(u, "second", "u-big.bin", content("u-big", 9 * MIB + 3), True)
    put(u, "second", "u-small.txt", content("u-small", 1234), False)

    # A copy: two files share one chunked blob.
    status, _, _ = t.request("COPY", t.dav_path("twenty.bin"), None,
                             {"Destination": URL + t.dav_path("twenty-copy.bin"), "Overwrite": "F"})
    check("seed COPY twenty.bin", status, 201)
    data = content("twenty", 20 * MIB)
    state["files"].append({"owner": "tester", "name": "twenty-copy.bin", "size": len(data), "sha": sha(data), "big": True})

    # JMAP: a 20 MiB upload made into a file, and a 12 MiB upload left unused.
    data = content("jmap20", 20 * MIB)
    up = t.upload(data)
    resp = t.jmap([FILES], [["FileNode/set", {"accountId": t.account, "create": {
        "n1": {"@type": "File", "name": "jmap20.bin", "blobId": up["blobId"], "mediaType": "application/octet-stream"}}}, "0"]])
    check("seed FileNode/set jmap20.bin", "n1" in resp[0][1].get("created", {}), True)
    state["files"].append({"owner": "tester", "name": "jmap20.bin", "size": len(data), "sha": sha(data), "big": True})
    data = content("unused12", 12 * MIB)
    up = t.upload(data)
    state["uploads"].append({"owner": "tester", "blobId": up["blobId"], "size": len(data), "sha": sha(data)})

    # Mail: one with a 10 MiB attachment (imported from a chunked upload) and a normal one.
    resp = t.jmap([MAIL], [["Mailbox/get", {"accountId": t.account}, "0"]])
    inbox = next(m["id"] for m in resp[0][1]["list"] if m.get("role") == "inbox")
    for subject, attachment in (("big attachment", content("attach", 10 * MIB)), ("normal mail", None)):
        raw = eml(subject, attachment)
        up = t.upload(raw, "message/rfc822")
        resp = t.jmap([MAIL], [["Email/import", {"accountId": t.account, "emails": {
            "e1": {"blobId": up["blobId"], "mailboxIds": {inbox: True}, "keywords": {}}}}, "0"]])
        check(f"seed Email/import {subject!r}", "e1" in resp[0][1].get("created", {}), True)
        state["mail"].append({"subject": subject, "size": len(raw), "sha": sha(raw),
                              "attachment_sha": sha(attachment) if attachment else None})

    # A calendar event and a contact.
    resp = t.jmap(["urn:ietf:params:jmap:calendars"], [["Calendar/get", {"accountId": t.account}, "0"]])
    cal = resp[0][1]["list"][0]["id"]
    resp = t.jmap(["urn:ietf:params:jmap:calendars"], [["CalendarEvent/set", {"accountId": t.account, "create": {
        "c1": {"@type": "Event", "calendarIds": {cal: True}, "title": "Compat check", "start": "2026-11-20T10:00:00",
               "timeZone": "Europe/Istanbul", "duration": "PT1H"}}}, "0"]])
    check("seed CalendarEvent/set", "c1" in resp[0][1].get("created", {}), True)
    resp = t.jmap(["urn:ietf:params:jmap:contacts"], [["AddressBook/get", {"accountId": t.account}, "0"]])
    book = resp[0][1]["list"][0]["id"]
    resp = t.jmap(["urn:ietf:params:jmap:contacts"], [["ContactCard/set", {"accountId": t.account, "create": {
        "k1": {"@type": "Card", "version": "1.0", "addressBookIds": {book: True}, "name": {"full": "Ada Compat"}}}}, "0"]])
    check("seed ContactCard/set", "k1" in resp[0][1].get("created", {}), True)

    # Share links.
    nodes, _ = file_nodes(t)
    for name in ("twenty.bin", "tiny.txt"):
        body = json.dumps({"fileId": nodes[name]["id"]}).encode()
        status, data, _ = t.request("POST", "/api/sharelink/create", body, {"Content-Type": "application/json"})
        check(f"seed share link on {name}", status, 200)
        state["links"].append(json.loads(data)["path"])

    for owner, client in (("tester", t), ("second", u)):
        nodes, file_state = file_nodes(client)
        mails, email_state = mail_state(client)
        state["accounts"][owner] = {
            "file_ids": {name: n["id"] for name, n in nodes.items()},
            "file_state": file_state,
            "email_state": email_state,
            "quota": quota_used(client),
        }
    with open(state_path, "w") as f:
        json.dump(state, f, indent=1)
    verify_all(server_dir, state, "all")


def verify_all(server_dir, state, mode):
    clients = {"tester": Client(server_dir, "tester"), "second": Client(server_dir, "second")}
    for c in clients.values():
        c.session()
    nodes = {owner: file_nodes(c)[0] for owner, c in clients.items()}

    # Files, over WebDAV and over JMAP download.
    deleted_size = {owner: 0 for owner in clients}
    destroyed_ids = {owner: set() for owner in clients}
    for f in state["files"]:
        c = clients[f["owner"]]
        gone = mode == "nuke" and f["big"]
        status, data, _ = c.request("GET", c.dav_path(f["name"]))
        if gone:
            check(f"{f['owner']}/{f['name']} deleted (WebDAV 404)", status, 404)
            check(f"{f['owner']}/{f['name']} not listed", f["name"] in nodes[f["owner"]], False)
            deleted_size[f["owner"]] += f["size"]
            destroyed_ids[f["owner"]].add(state["accounts"][f["owner"]]["file_ids"][f["name"]])
            continue
        check(f"{f['owner']}/{f['name']} WebDAV bytes", (status, sha(data)), (200, f["sha"]))
        node = nodes[f["owner"]].get(f["name"])
        if node is None:
            bad(f"{f['owner']}/{f['name']} missing from FileNode/query")
            continue
        status, data, _ = c.request("GET", f"/jmap/download/{c.account}/{node['blobId']}/x?accept=application/octet-stream")
        check(f"{f['owner']}/{f['name']} JMAP bytes", (status, sha(data)), (200, f["sha"]))

    # Mail, with the raw message (attachment included).
    t = clients["tester"]
    mails, _ = mail_state(t)
    for m in state["mail"]:
        e = mails.get(m["subject"])
        if e is None:
            bad(f"mail {m['subject']!r} missing")
            continue
        status, data, _ = t.request("GET", f"/jmap/download/{t.account}/{e['blobId']}/m.eml")
        check(f"mail {m['subject']!r} raw message bytes", (status, sha(data)), (200, m["sha"]))
        if m["attachment_sha"]:
            resp = t.jmap([MAIL], [["Email/get", {"accountId": t.account, "ids": [e["id"]], "properties": ["attachments"]}, "0"]])
            att = resp[0][1]["list"][0]["attachments"][0]
            status, data, _ = t.request("GET", f"/jmap/download/{t.account}/{att['blobId']}/big.bin")
            check(f"mail {m['subject']!r} attachment bytes", (status, sha(data)), (200, m["attachment_sha"]))

    # Calendar and contacts.
    resp = t.jmap(["urn:ietf:params:jmap:calendars"], [["CalendarEvent/query", {"accountId": t.account}, "0"],
        ["CalendarEvent/get", {"accountId": t.account, "#ids": {"resultOf": "0", "name": "CalendarEvent/query", "path": "/ids"}, "properties": ["title"]}, "1"]])
    check("calendar event present", [e["title"] for e in resp[1][1]["list"]], ["Compat check"])
    resp = t.jmap(["urn:ietf:params:jmap:contacts"], [["ContactCard/query", {"accountId": t.account}, "0"],
        ["ContactCard/get", {"accountId": t.account, "#ids": {"resultOf": "0", "name": "ContactCard/query", "path": "/ids"}, "properties": ["name"]}, "1"]])
    check("contact present", [c.get("name", {}).get("full") for c in resp[1][1]["list"]], ["Ada Compat"])

    # Quotas and change logs. After nuke the quota must equal what official
    # Stalwart shows after deleting the same files by hand (REF_QUOTA, made by
    # `delete-big`): a file's quota covers its content and its metadata record.
    reference = None
    if mode == "nuke" and os.environ.get("REF_QUOTA"):
        with open(os.environ["REF_QUOTA"]) as f:
            reference = json.load(f)
    for owner, c in clients.items():
        before = state["accounts"][owner]["quota"]
        after = quota_used(c)
        if mode == "all":
            check(f"{owner} quota unchanged", after, before)
        else:
            if reference is not None:
                check(f"{owner} quota equals official deleting the same files", after, reference[owner])
            drop = sum(before.values()) - sum(after.values())
            meta = drop - deleted_size[owner]
            files = len(destroyed_ids[owner])
            check(f"{owner} quota dropped by the deleted sizes plus a metadata record per file "
                  f"(content {deleted_size[owner]}, metadata {meta} for {files} files)",
                  0 <= meta <= 512 * files, True)
        resp = c.jmap([FILES], [["FileNode/changes", {"accountId": c.account,
                                                      "sinceState": state["accounts"][owner]["file_state"]}, "0"]])
        r = resp[0][1]
        check(f"{owner} FileNode/changes destroyed", sorted(r.get("destroyed", [])), sorted(destroyed_ids[owner]))
        check(f"{owner} FileNode/changes created+updated", (r.get("created", []), r.get("updated", [])), ([], []))

    return FAILURES


def roundtrip(server_dir):
    c = Client(server_dir, "tester")
    c.session()
    one, two = content("rt1", 3 * MIB), content("rt2", 5000)
    status, _, _ = c.request("PUT", c.dav_path("roundtrip.bin"), one)
    check("roundtrip upload", status, 201)
    status, data, _ = c.request("GET", c.dav_path("roundtrip.bin"))
    check("roundtrip read", (status, sha(data)), (200, sha(one)))
    status, _, _ = c.request("PUT", c.dav_path("roundtrip.bin"), two)
    check("roundtrip replace", status, 204)
    status, data, _ = c.request("GET", c.dav_path("roundtrip.bin"))
    check("roundtrip read after replace", (status, sha(data)), (200, sha(two)))
    status, _, _ = c.request("DELETE", c.dav_path("roundtrip.bin"))
    check("roundtrip delete", status, 204)
    status, _, _ = c.request("GET", c.dav_path("roundtrip.bin"))
    check("roundtrip gone", status, 404)


def delete_big(server_dir, state, out_path):
    """Reference for nuke: delete the big files by hand (WebDAV) and save the quotas."""
    clients = {"tester": Client(server_dir, "tester"), "second": Client(server_dir, "second")}
    for f in state["files"]:
        if f["big"]:
            c = clients[f["owner"]]
            status, _, _ = c.request("DELETE", c.dav_path(f["name"]))
            check(f"reference delete {f['owner']}/{f['name']}", status, 204)
    quotas = {}
    for owner, c in clients.items():
        c.session()
        quotas[owner] = quota_used(c)
    with open(out_path, "w") as f:
        json.dump(quotas, f)


def links(server_dir, state, want):
    c = Client(server_dir, "tester")
    for path in state["links"]:
        status, _, headers = c.request("GET", path, auth=False)
        check(f"old share link {path[:8]}… answers", status, want)


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "seed":
        seed(sys.argv[2], sys.argv[3])
    elif cmd == "verify":
        with open(sys.argv[3]) as f:
            verify_all(sys.argv[2], json.load(f), sys.argv[4])
    elif cmd == "roundtrip":
        roundtrip(sys.argv[2])
    elif cmd == "delete-big":
        with open(sys.argv[3]) as f:
            delete_big(sys.argv[2], json.load(f), sys.argv[4])
    elif cmd == "links":
        with open(sys.argv[3]) as f:
            links(sys.argv[2], json.load(f), int(sys.argv[4]))
    sys.exit(1 if FAILURES else 0)
