// rvrelay: production bridge between the Radmin service named pipe and the
// native Linux front end. It runs inside the same Wine prefix as the service,
// connects to \\.\pipe\RadminVpnGuiChannel as the GUI would, and relays raw
// bytes between that pipe and its own stdin/stdout. The Rust app spawns it with
// `wine rvrelay.exe` and both pipes wired to the child, so no TCP port is
// opened and nothing but this process touches the Wine pipe.
//
// Unlike pipespy, there is no second pipe and no logging: it must never write
// the user's traffic anywhere. The app is the only client; the real GUI must
// not be running at the same time (two clients on one pipe would race).
//
// Build (dev): zig cc -target x86-windows-gnu -O2 -o rvrelay.exe bridge/rvrelay.c
#include <windows.h>

#define PIPE_NAME "\\\\.\\pipe\\RadminVpnGuiChannel"
#define BUF 4096

static HANDLE g_pipe;
static HANDLE g_out;

// stdin -> pipe: app commands flow to the service.
static DWORD WINAPI pump_in(LPVOID arg) {
    HANDLE in = (HANDLE)arg;
    unsigned char buf[BUF];
    DWORD n, w;
    while (ReadFile(in, buf, sizeof buf, &n, NULL) && n > 0) {
        unsigned char *p = buf;
        DWORD left = n;
        while (left > 0) {
            if (!WriteFile(g_pipe, p, left, &w, NULL) || w == 0) return 0;
            p += w;
            left -= w;
        }
    }
    return 0;
}

int main(void) {
    HANDLE in = GetStdHandle(STD_INPUT_HANDLE);
    g_out = GetStdHandle(STD_OUTPUT_HANDLE);

    // The service may not have created the pipe yet; wait briefly for it.
    for (int tries = 0; tries < 100; tries++) {
        g_pipe = CreateFileA(PIPE_NAME, GENERIC_READ | GENERIC_WRITE, 0, NULL,
                             OPEN_EXISTING, 0, NULL);
        if (g_pipe != INVALID_HANDLE_VALUE) break;
        if (GetLastError() == ERROR_PIPE_BUSY)
            WaitNamedPipeA(PIPE_NAME, 1000);
        else
            Sleep(100);
    }
    if (g_pipe == INVALID_HANDLE_VALUE) return 1;

    // The GUI uses the pipe as a byte stream; leave the default read mode.
    CreateThread(NULL, 0, pump_in, in, 0, NULL);

    // pipe -> stdout: service events and replies flow to the app.
    unsigned char buf[BUF];
    DWORD n, w;
    while (ReadFile(g_pipe, buf, sizeof buf, &n, NULL) && n > 0) {
        unsigned char *p = buf;
        DWORD left = n;
        while (left > 0) {
            if (!WriteFile(g_out, p, left, &w, NULL) || w == 0) return 0;
            p += w;
            left -= w;
        }
    }
    return 0;
}
