import argparse
import base64
from bisect import bisect_left, bisect_right
import shlex
import socket
import socketserver
import threading
import time
from email.utils import formatdate

THREAD = [28, 34, 55, 66, 73, 77, 78, 81, 83, 85]
USER = b"robin@demo.test"
PASSWORD = b"local-demo-only"

def sample(uid, latest_paragraphs, earlier_paragraphs, now):
    if uid in THREAD:
        index = THREAD.index(uid)
        subject = "Field test notes" if index == 0 else "Re: Field test notes"
        previous = THREAD[index - 1]
        references = (
            f"In-Reply-To: <mail-{previous}@demo.test>\r\n"
            f"References: <mail-28@demo.test> <mail-{previous}@demo.test>\r\n"
            if index > 0
            else ""
        )
    else:
        subjects = ("Route review", "Weekly planning", "Equipment list", "Pilot summary")
        subject = f"{subjects[uid % len(subjects)]} · {uid}"
        references = ""
    header = (
        f"From: Avery Lane <avery@demo.test>\r\n"
        f"To: Robin Ellis <robin@demo.test>\r\n"
        f"Date: {formatdate(now - 180 * (THREAD[-1] - uid), usegmt=True)}\r\n"
        f"Subject: {subject}\r\n"
        f"Message-ID: <mail-{uid}@demo.test>\r\n"
        f"{references}Content-Type: text/plain; charset=utf-8\r\n\r\n"
    ).encode()
    body = (
        f"Observation {uid}\r\n\r\n"
        "The latest field test is ready to review. The team compared the two routes "
        "and noted where visibility improved.\r\n\r\n"
        "Please check the annotated map before the next trial.\r\n"
    ).encode()
    paragraphs = latest_paragraphs if uid == THREAD[-1] else earlier_paragraphs if uid in THREAD else 0
    for index in range(paragraphs):
        body += (
            f"\r\n\r\nCheckpoint {index + 1}\r\n"
            "Compare the access road and the trail crossing. Take a fresh photo "
            "from the overlook and include it in the next field report."
        ).encode()
    return header, body




def uids_in_set(items, expression):
    end = items[-1] if items else 0
    found = set()
    for term in expression.split(","):
        bounds = [end if value == "*" else int(value) for value in term.split(":")]
        low, high = min(bounds), max(bounds)
        found.update(items[bisect_left(items, low):bisect_right(items, high)])
    return sorted(found)


class ImapDemo(socketserver.StreamRequestHandler):
    def send(self, text):
        self.wfile.write(text.encode())

    def handle(self):
        self.request.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.send("* OK IMAP demo ready\r\n")
        items = []
        body_calls = 0
        header_calls = 0
        while line := self.rfile.readline():
            words = shlex.split(line.decode())
            tag, verb, *args = words
            verb = verb.upper()
            print(verb, " ".join(args) if verb != "LOGIN" else "", flush=True)
            if verb == "LOGIN":
                length = args[-1]
                for _ in range(2):
                    self.send("+ Ready for literal\r\n")
                    self.rfile.read(int(length.strip("{}")))
                    length = self.rfile.readline().decode().strip()
            elif verb == "CAPABILITY":
                self.send("* CAPABILITY IMAP4rev1\r\n")
            elif verb == "LIST":
                self.send('* LIST (\\HasNoChildren) "/" "INBOX"\r\n')
            elif verb in ("EXAMINE", "SELECT"):
                items = self.server.uids if args[0].upper() == "INBOX" else []
                uidnext = (items[-1] + 1) if items else 1
                self.send(
                    f"* {len(items)} EXISTS\r\n"
                    f"* OK [UIDVALIDITY 2] Stable demo mailbox\r\n"
                    f"* OK [UIDNEXT {uidnext}] Next UID\r\n"
                )
            elif verb == "UID" and args[0].upper() == "SEARCH":
                found = uids_in_set(items, args[-1])
                self.send("* SEARCH" + "".join(f" {uid}" for uid in found) + "\r\n")
            elif verb in ("UID", "FETCH"):
                expression = args[1] if verb == "UID" else args[0]
                query = " ".join(args).upper()
                if "BODY.PEEK[HEADER]" in query:
                    print(f"HEADER_BATCH {expression}", flush=True)
                    delay = self.server.header_delay
                    if header_calls == 0 and self.server.first_header_delay is not None:
                        delay = self.server.first_header_delay
                    time.sleep(delay)
                    header_calls += 1
                if "BODY.PEEK[]" in query:
                    delay = self.server.first_delay if body_calls == 0 else self.server.body_delay
                    print(f"BODY_BATCH {body_calls + 1} {expression} {delay}s", flush=True)
                    while delay > 4:
                        time.sleep(4)
                        self.send("* OK Waiting to send the synthetic body\r\n")
                        delay -= 4
                    time.sleep(delay)
                    body_calls += 1
                for uid in uids_in_set(items, expression):
                    header, body = self.server.mail[uid]
                    unread = uid % 3 != 0 or (self.server.unread_thread and uid in THREAD)
                    flags = "" if unread else r"\Seen"
                    fields = f"UID {uid} FLAGS ({flags}) RFC822.SIZE {len(header) + len(body)}"
                    if "BODY" not in query:
                        self.send(f"* {uid} FETCH ({fields})\r\n")
                        continue
                    if "HEADER.FIELDS" in query:
                        section, literal = "HEADER.FIELDS (MESSAGE-ID)", f"Message-ID: <mail-{uid}@demo.test>\r\n\r\n".encode()
                    elif "HEADER" in query:
                        section, literal = "HEADER", header
                    else:
                        section, literal = "", header + body
                    self.send(f"* {uid} FETCH ({fields} BODY[{section}] {{{len(literal)}}}\r\n")
                    self.wfile.write(literal)
                    self.send(")\r\n")
            elif verb == "LOGOUT":
                self.send(f"* BYE\r\n{tag} OK LOGOUT\r\n")
                return
            elif verb not in ("LOGIN", "NOOP"):
                self.send(f"{tag} BAD Unsupported demo command\r\n")
                continue
            self.send(f"{tag} OK {verb}\r\n")


class SmtpDemo(socketserver.StreamRequestHandler):
    def send(self, text):
        self.wfile.write((text + "\r\n").encode())

    def handle(self):
        self.send("220 localhost ESMTP")
        while line := self.rfile.readline():
            line = line.rstrip(b"\r\n")
            if line.startswith(b"EHLO "):
                self.send("250-localhost")
                self.send("250 AUTH PLAIN")
            elif line.startswith(b"AUTH PLAIN "):
                time.sleep(self.server.check_delay)
                offered = base64.b64decode(line.split()[-1])
                if offered == b"\0" + USER + b"\0" + PASSWORD:
                    self.send("235 2.7.0 Authenticated")
                else:
                    self.send("535 5.7.8 Bad synthetic credentials")
            elif line == b"QUIT":
                self.send("221 Bye")
                return
            else:
                self.send("502 Login-only fixture")


class LoopbackServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    parser = argparse.ArgumentParser(description="Synthetic login and inbox fixture")
    parser.add_argument("--imap-port", type=int, default=1143)
    parser.add_argument("--smtp-port", type=int, default=1025)
    parser.add_argument("--header-delay", type=float, default=0.)
    parser.add_argument("--first-header-delay", type=float)
    parser.add_argument("--first-body-delay", type=float, default=2.2)
    parser.add_argument("--body-delay", type=float, default=1.15)
    parser.add_argument("--check-delay", type=float, default=1.05)
    parser.add_argument("--latest-paragraphs", type=int, default=0)
    parser.add_argument("--messages", type=int, default=85, help="mailbox size, e.g. 31882")
    parser.add_argument("--earlier-paragraphs", type=int, default=0)
    parser.add_argument("--unread-thread", action="store_true", help="mark all ten latest-thread messages unread")
    args = parser.parse_args()
    if args.messages < 85 or min(
        args.header_delay, args.first_body_delay, args.body_delay, args.check_delay, args.latest_paragraphs, args.earlier_paragraphs
    ) < 0:
        parser.error("provide at least 85 messages and nonnegative delays")

    THREAD[:] = [uid + args.messages - 85 for uid in THREAD]
    with (
        LoopbackServer(("127.0.0.1", args.imap_port), ImapDemo) as imap,
        LoopbackServer(("127.0.0.1", args.smtp_port), SmtpDemo) as smtp,
    ):
        now = time.time()
        imap.uids = range(1, args.messages + 1)
        imap.mail = {
            uid: sample(uid, args.latest_paragraphs, args.earlier_paragraphs, now)
            for uid in imap.uids
        }
        imap.unread_thread = args.unread_thread
        imap.header_delay = args.header_delay
        imap.first_header_delay = args.first_header_delay
        imap.first_delay = args.first_body_delay
        imap.body_delay = args.body_delay
        smtp.check_delay = args.check_delay
        threading.Thread(target=smtp.serve_forever, daemon=True).start()
        print(f"PORTS {imap.server_address[1]} {smtp.server_address[1]}", flush=True)
        imap.serve_forever()


if __name__ == "__main__":
    main()
