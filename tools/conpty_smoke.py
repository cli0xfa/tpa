"""Windows ConPTY black-box check. No GUI window or third-party Python modules.

python tools/conpty_smoke.py target/release/tpa.exe target/verification/soak.mp4
"""
import codecs
import ctypes as c
from ctypes import wintypes as w
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
import unicodedata

CSI = re.compile(r"\x1b\[([0-?]*)([ -/]*)([@-~])")

if os.name != "nt":
    raise SystemExit("This verification harness requires Windows 10+.")

k = c.WinDLL("kernel32", use_last_error=True)


class Coord(c.Structure):
    _fields_ = [("X", w.SHORT), ("Y", w.SHORT)]


class Startup(c.Structure):
    _fields_ = [
        ("cb", w.DWORD), ("reserved", w.LPWSTR), ("desktop", w.LPWSTR),
        ("title", w.LPWSTR), ("x", w.DWORD), ("y", w.DWORD),
        ("xsize", w.DWORD), ("ysize", w.DWORD), ("xchars", w.DWORD),
        ("ychars", w.DWORD), ("fill", w.DWORD), ("flags", w.DWORD),
        ("show", w.WORD), ("reserved2size", w.WORD), ("reserved2", c.c_void_p),
        ("stdin", w.HANDLE), ("stdout", w.HANDLE), ("stderr", w.HANDLE),
    ]


class StartupEx(c.Structure):
    _fields_ = [("startup", Startup), ("attributes", c.c_void_p)]


class ProcessInfo(c.Structure):
    _fields_ = [("process", w.HANDLE), ("thread", w.HANDLE), ("pid", w.DWORD), ("tid", w.DWORD)]


def api(name, args, result):
    function = getattr(k, name)
    function.argtypes, function.restype = args, result
    return function


create_pipe = api("CreatePipe", [c.POINTER(w.HANDLE), c.POINTER(w.HANDLE), c.c_void_p, w.DWORD], w.BOOL)
create_console = api("CreatePseudoConsole", [Coord, w.HANDLE, w.HANDLE, w.DWORD, c.POINTER(w.HANDLE)], c.c_long)
resize_console = api("ResizePseudoConsole", [w.HANDLE, Coord], c.c_long)
close_console = api("ClosePseudoConsole", [w.HANDLE], None)
initialize = api("InitializeProcThreadAttributeList", [c.c_void_p, w.DWORD, w.DWORD, c.POINTER(c.c_size_t)], w.BOOL)
update = api("UpdateProcThreadAttribute", [c.c_void_p, w.DWORD, c.c_size_t, c.c_void_p, c.c_size_t, c.c_void_p, c.c_void_p], w.BOOL)
delete = api("DeleteProcThreadAttributeList", [c.c_void_p], None)
create_process = api("CreateProcessW", [w.LPCWSTR, w.LPWSTR, c.c_void_p, c.c_void_p, w.BOOL, w.DWORD, c.c_void_p, w.LPCWSTR, c.POINTER(StartupEx), c.POINTER(ProcessInfo)], w.BOOL)
read_file = api("ReadFile", [w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p], w.BOOL)
write_file = api("WriteFile", [w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p], w.BOOL)
close = api("CloseHandle", [w.HANDLE], w.BOOL)
wait = api("WaitForSingleObject", [w.HANDLE, w.DWORD], w.DWORD)
exit_code = api("GetExitCodeProcess", [w.HANDLE, c.POINTER(w.DWORD)], w.BOOL)
terminate = api("TerminateProcess", [w.HANDLE, w.UINT], w.BOOL)
get_std = api("GetStdHandle", [w.DWORD], w.HANDLE)
set_std = api("SetStdHandle", [w.DWORD, w.HANDLE], w.BOOL)


def checked(result):
    if not result:
        raise c.WinError(c.get_last_error())
    return result


class Screen:
    """Decode enough VT to reconstruct the header/status, ignoring video rows."""
    def __init__(self, width=160, height=51):
        self.width, self.height = width, height
        self.rows = {}
        self.x = self.y = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.bytes = 0
        self.rgb_sequences = 0
        self.reply = None
        self.queries = []
        self.lock = threading.Lock()

    def resize(self, width, height):
        with self.lock:
            self.width, self.height = width, height
            self.rows.clear()
            self.x = self.y = 0

    def feed(self, data):
        with self.lock:
            self.bytes += len(data)
            text = self.pending + self.decoder.decode(data)
            self.pending = ""
            i = 0
            while i < len(text):
                if text[i] == "\x1b":
                    match = CSI.match(text, i)
                    if match:
                        parameters, _, command = match.groups()
                        values = [int(v) if v.isdigit() else 0 for v in parameters.lstrip("?<=>").split(";")]
                        if command in "Hf":
                            self.y = (values[0] or 1) - 1
                            self.x = ((values[1] if len(values) > 1 else 1) or 1) - 1
                        elif command == "G":
                            self.x = (values[0] or 1) - 1
                        elif command == "J" and values[0] in (2, 3):
                            self.rows.clear()
                        elif command == "m" and "38;2;" in parameters:
                            self.rgb_sequences += 1
                        elif command in "nct" and self.reply:
                            self.queries.append(parameters + command)
                            if command == "n" and values[0] == 6:
                                self.reply(f"\x1b[{min(self.y + 1, self.height)};{min(self.x + 1, self.width)}R".encode())
                            elif command == "c":
                                self.reply(b"\x1b[?1;2c")
                            elif command == "t" and values[0] == 18:
                                self.reply(f"\x1b[8;{self.height};{self.width}t".encode())
                        i = match.end()
                        continue
                    if text.startswith("\x1b]", i):
                        ends = [v for v in (text.find("\x07", i), text.find("\x1b\\", i)) if v >= 0]
                        if not ends:
                            self.pending = text[i:]
                            break
                        end = min(ends)
                        i = end + (2 if text[end] == "\x1b" else 1)
                        continue
                    if i + 1 == len(text) or text[i + 1] == "[":
                        self.pending = text[i:]
                        break
                    i += 2
                    continue
                end = text.find("\x1b", i)
                if end < 0:
                    end = len(text)
                segment = text[i:end]
                if self.y == 0 or self.y >= self.height - 8:
                    row = self.rows.setdefault(self.y, [" "] * self.width)
                    for char in segment:
                        if char == "\r":
                            self.x = 0
                        elif char == "\n":
                            self.y += 1
                        elif not char.isprintable():
                            continue
                        else:
                            width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
                            if 0 <= self.x < self.width:
                                row[self.x] = char
                            if width == 2 and 0 <= self.x + 1 < self.width:
                                row[self.x + 1] = ""
                            self.x += width
                i = end

    def status(self):
        with self.lock:
            visible_footer = range(max(0, self.height - 8), self.height)
            for row_index in visible_footer:
                row = "".join(self.rows.get(row_index, [])).lstrip()
                if row.startswith(("加载", "播放", "暂停", "缓冲", "结束", "播放失败")):
                    return row
            return "".join(self.rows.get(self.height - 3, []))

    def header(self):
        with self.lock:
            return "".join(self.rows.get(0, []))


def main():
    executable, video = (str(Path(arg).resolve()) for arg in sys.argv[1:3])
    input_read, input_write, output_read, output_write, console = (w.HANDLE() for _ in range(5))
    checked(create_pipe(c.byref(input_read), c.byref(input_write), None, 0))
    checked(create_pipe(c.byref(output_read), c.byref(output_write), None, 0))
    assert create_console(Coord(160, 51), input_read, output_write, 0, c.byref(console)) == 0
    size = c.c_size_t()
    initialize(None, 1, 0, c.byref(size))
    attributes = c.create_string_buffer(size.value)
    checked(initialize(attributes, 1, 0, c.byref(size)))
    checked(update(attributes, 0, 0x00020016, console, c.sizeof(console), None, None))
    startup = StartupEx()
    startup.startup.cb = c.sizeof(startup)
    startup.attributes = c.cast(attributes, c.c_void_p)
    process = ProcessInfo()
    command = c.create_unicode_buffer(subprocess.list2cmdline([executable, "play", video, "--color", "truecolor", *sys.argv[3:]]))
    # Redirected handles in the host must not replace the new console's handles.
    saved_handles = {value: get_std(value) for value in (-10, -11, -12)}
    try:
        for value in saved_handles:
            checked(set_std(value, None))
        checked(create_process(executable, command, None, None, False, 0x00080000, None, str(Path.cwd()), c.byref(startup), c.byref(process)))
    finally:
        for value, handle in saved_handles.items():
            checked(set_std(value, handle))
    close(input_read)
    close(output_write)
    close(process.thread)
    delete(attributes)
    screen = Screen()
    errors = []

    def drain():
        try:
            buffer = c.create_string_buffer(65536)
            count = w.DWORD()
            while read_file(output_read, buffer, len(buffer), c.byref(count), None) and count.value:
                screen.feed(buffer.raw[:count.value])
        except Exception as error:
            errors.append(str(error))

    def send(keys):
        count = w.DWORD()
        checked(write_file(input_write, keys, len(keys), c.byref(count), None))

    screen.reply = send
    reader = threading.Thread(target=drain, daemon=True)
    reader.start()

    def expect(word, header=False):
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline:
            if word in (screen.header() if header else screen.status()):
                return
            time.sleep(0.05)
        raise AssertionError(f"expected {word!r}, status={screen.status()!r}, errors={errors}, pending={screen.pending[:100]!r}, bytes={screen.bytes}, rgb={screen.rgb_sequences}")

    result = {"pid": process.pid, "terminal": "Windows ConPTY", "size": "160x51 (video 160x45)"}
    try:
        expect("播放")
        time.sleep(8)
        result["playing_status"] = screen.status().strip()
        print(json.dumps({"initial_playback": result["playing_status"]}, ensure_ascii=False), flush=True)
        send(b" ")
        expect("暂停")
        send(b"2")
        expect("盲文")
        send(b"3")
        expect("ASCII")
        send(b"1 ")
        expect("播放")
        print(json.dumps({"after_modes": screen.status().strip()}, ensure_ascii=False), flush=True)
        send(b"\x1b[D\x1b[C")
        time.sleep(1)
        print(json.dumps({"after_seek": screen.status().strip()}, ensure_ascii=False), flush=True)
        assert resize_console(console, Coord(20, 8)) == 0
        screen.resize(20, 8)
        time.sleep(2)
        assert resize_console(console, Coord(160, 51)) == 0
        screen.resize(160, 51)
        expect("播放")
        send(b" ")
        expect("暂停")
        assert resize_console(console, Coord(80, 24)) == 0
        screen.resize(80, 24)
        time.sleep(0.4)
        assert resize_console(console, Coord(160, 51)) == 0
        screen.resize(160, 51)
        expect("暂停")
        send(b"\x1b")
        expect("终端播放一切", header=True)
        send(b"/" + Path(video).name.encode("utf-8") + b"\r\r")
        expect("播放")
        send(b"?")
        time.sleep(0.2)
        send(b"\x1b")
        time.sleep(0.2)
        send(b"q")
        assert wait(process.process, 5000) == 0, "player did not quit"
        code = w.DWORD()
        checked(exit_code(process.process, c.byref(code)))
        assert code.value == 0
        result.update(exit_code=code.value, output_bytes=screen.bytes, truecolor_sequences=screen.rgb_sequences, queries=screen.queries, checks="pause, modes, seek, resize, automatic pause, preserved manual pause, browser, filter, reopen, help, quit")
        assert screen.rgb_sequences > 0, "explicit truecolor was suppressed"
        assert not errors, errors
        print(json.dumps(result, ensure_ascii=False, indent=2))
    finally:
        if wait(process.process, 0) != 0:
            terminate(process.process, 1)
        close_console(console)
        reader.join(3)
        close(input_write)
        close(output_read)
        close(process.process)


if __name__ == "__main__":
    main()
