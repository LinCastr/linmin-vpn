// pipespy: development tool. Listens on \\.\pipe\RadminVpnGuiChannex, connects
// to the real \\.\pipe\RadminVpnGuiChannel and relays both ways, logging every
// message as hex. Used with a copy of the GUI whose pipe name was patched.
#include <windows.h>
#include <stdio.h>

static FILE *logf;
static CRITICAL_SECTION cs;

typedef struct { HANDLE from, to; const char *dir; } relay_t;

static void dump(const char *dir, const unsigned char *b, DWORD n) {
    EnterCriticalSection(&cs);
    SYSTEMTIME t; GetLocalTime(&t);
    fprintf(logf, "%02d:%02d:%02d.%03d %s %lu\n", t.wHour, t.wMinute, t.wSecond, t.wMilliseconds, dir, n);
    for (DWORD i = 0; i < n; i += 16) {
        fprintf(logf, "  %04lx ", i);
        for (DWORD j = 0; j < 16; j++) i + j < n ? fprintf(logf, "%02x ", b[i + j]) : fprintf(logf, "   ");
        fprintf(logf, " ");
        for (DWORD j = 0; j < 16 && i + j < n; j++) fputc(b[i + j] >= 32 && b[i + j] < 127 ? b[i + j] : '.', logf);
        fputc('\n', logf);
    }
    fflush(logf);
    LeaveCriticalSection(&cs);
}

static DWORD WINAPI relay(LPVOID p) {
    relay_t *r = p;
    static unsigned char buf[1 << 20];
    unsigned char *b = HeapAlloc(GetProcessHeap(), 0, 1 << 20);
    (void)buf;
    for (;;) {
        DWORD n = 0, total = 0;
        BOOL ok;
        do {
            ok = ReadFile(r->from, b + total, (1 << 20) - total, &n, NULL);
            total += n;
        } while (!ok && GetLastError() == ERROR_MORE_DATA);
        if (!ok && total == 0) { fprintf(logf, "%s closed (%lu)\n", r->dir, GetLastError()); fflush(logf); ExitProcess(0); }
        dump(r->dir, b, total);
        DWORD w;
        if (!WriteFile(r->to, b, total, &w, NULL)) { fprintf(logf, "%s write failed (%lu)\n", r->dir, GetLastError()); fflush(logf); ExitProcess(0); }
    }
    return 0;
}

int main(int argc, char **argv) {
    InitializeCriticalSection(&cs);
    logf = fopen(argc > 1 ? argv[1] : "pipespy.log", "a");
    if (!logf) return 1;
    HANDLE srv = CreateNamedPipeW(L"\\\\.\\pipe\\RadminVpnGuiChannex",
        PIPE_ACCESS_DUPLEX, PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
        1, 1 << 20, 1 << 20, 0, NULL);
    if (srv == INVALID_HANDLE_VALUE) { fprintf(logf, "CreateNamedPipe %lu\n", GetLastError()); return 1; }
    fprintf(logf, "listening\n"); fflush(logf);
    if (!ConnectNamedPipe(srv, NULL) && GetLastError() != ERROR_PIPE_CONNECTED) { fprintf(logf, "ConnectNamedPipe %lu\n", GetLastError()); return 1; }
    HANDLE cli;
    for (;;) {
        cli = CreateFileW(L"\\\\.\\pipe\\RadminVpnGuiChannel", GENERIC_READ | GENERIC_WRITE, 0, NULL, OPEN_EXISTING, 0, NULL);
        if (cli != INVALID_HANDLE_VALUE) break;
        if (GetLastError() != ERROR_PIPE_BUSY || !WaitNamedPipeW(L"\\\\.\\pipe\\RadminVpnGuiChannel", 5000)) {
            fprintf(logf, "connect real %lu\n", GetLastError()); return 1;
        }
    }
    DWORD mode = PIPE_READMODE_MESSAGE;
    BOOL m = SetNamedPipeHandleState(cli, &mode, NULL, NULL);
    DWORD info = 0, ob = 0, ib = 0, mx = 0;
    GetNamedPipeInfo(cli, &info, &ob, &ib, &mx);
    fprintf(logf, "connected; message mode %s; server pipe flags 0x%lx out %lu in %lu max %lu\n", m ? "ok" : "failed", info, ob, ib, mx);
    fflush(logf);
    static relay_t a, b;
    a = (relay_t){ srv, cli, "GUI->SVC" };
    b = (relay_t){ cli, srv, "SVC->GUI" };
    HANDLE t = CreateThread(NULL, 0, relay, &a, 0, NULL);
    relay(&b);
    WaitForSingleObject(t, INFINITE);
    return 0;
}
