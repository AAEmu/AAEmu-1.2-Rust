// ============================================================================
//  video.cpp - constant-frame-rate video of the client, from inside its Present hook.
//
//  Render thread (Present hook): copy the back buffer into one of kSlots staging textures
//  (an asynchronous GPU copy that never waits), then read back the frame from two presents
//  earlier with a non-blocking Map and hand it to the writer through a small buffer pool.
//  If the pool is empty or the GPU is not done in time the frame is dropped and counted: the
//  render thread never waits for the encoder.
//
//  Writer thread: turns the irregular game frames into a constant frame rate (CfrScheduler)
//  and pipes raw BGRA to an ffmpeg child process (libx264, ultrafast).
//
//  The GPU objects are created, used and released on the render thread only.
// ============================================================================
#define WIN32_LEAN_AND_MEAN
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <d3d10_1.h>
#include <d3d11.h>
#include <dxgi.h>
#include <algorithm>
#include <atomic>
#include <deque>
#include <map>
#include <string>
#include <vector>
#include "tracecore.h"
#include "video.h"
#include "videocore.h"
#include "visuals.h"

namespace video {
static const int kSlots = 4;                  // staging textures: copy into n % 4, read n-2 (retry until n-3)
static const int kPool = 6;                   // frame buffers between the render thread and the writer
static const int64_t kLatencyUs = 250000;     // a frame presented longer ago than this has surely arrived

template <class T> struct Ref {
    T* p = nullptr;
    Ref() {}
    Ref(const Ref&) = delete;
    Ref& operator=(const Ref&) = delete;
    ~Ref() { reset(); }
    T* operator->() const { return p; }
    T** out() { reset(); return &p; }
    void reset() { if (p) { p->Release(); p = nullptr; } }
};

static bool supported_format(DXGI_FORMAT f) {
    return f == DXGI_FORMAT_R8G8B8A8_UNORM || f == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB ||
           f == DXGI_FORMAT_B8G8R8A8_UNORM || f == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB;
}
static bool is_rgba(DXGI_FORMAT f) { return f == DXGI_FORMAT_R8G8B8A8_UNORM || f == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB; }

// ---- GPU access (render thread only) -------------------------------------------------------
enum MapResult { MAP_OK, MAP_BUSY, MAP_FAIL };
struct Mapped { const BYTE* data; UINT pitch; };

class Gpu {
public:
    virtual ~Gpu() {}
    virtual bool init(IDXGISwapChain* chain, std::string* err) = 0;
    virtual bool copy_in(IDXGISwapChain* chain, int slot) = 0;
    virtual MapResult map(int slot, Mapped* out) = 0;
    virtual void unmap(int slot) = 0;
    UINT width = 0, height = 0;
    bool rgba = false;
};

class Gpu11 : public Gpu {
    Ref<ID3D11Device> dev;
    Ref<ID3D11DeviceContext> ctx;
    Ref<ID3D11Texture2D> staging[kSlots];
    Ref<ID3D11Texture2D> resolved;
    DXGI_FORMAT format = DXGI_FORMAT_UNKNOWN;
public:
    bool init(IDXGISwapChain* chain, std::string* err) override {
        if (FAILED(chain->GetDevice(__uuidof(ID3D11Device), (void**)dev.out()))) { *err = "not a DX11 swap chain"; return false; }
        dev->GetImmediateContext(ctx.out());
        Ref<ID3D11Texture2D> back;
        if (FAILED(chain->GetBuffer(0, __uuidof(ID3D11Texture2D), (void**)back.out()))) { *err = "cannot read the DX11 back buffer"; return false; }
        D3D11_TEXTURE2D_DESC d; back->GetDesc(&d);
        if (!supported_format(d.Format)) { *err = "unsupported DX11 pixel format"; return false; }
        format = d.Format; width = d.Width; height = d.Height; rgba = is_rgba(d.Format);
        D3D11_TEXTURE2D_DESC sd = d;
        sd.Usage = D3D11_USAGE_STAGING; sd.CPUAccessFlags = D3D11_CPU_ACCESS_READ; sd.BindFlags = 0; sd.MiscFlags = 0;
        sd.SampleDesc.Count = 1; sd.SampleDesc.Quality = 0; sd.MipLevels = 1; sd.ArraySize = 1;
        for (int i = 0; i < kSlots; ++i)
            if (FAILED(dev->CreateTexture2D(&sd, NULL, staging[i].out()))) { *err = "cannot allocate DX11 staging textures"; return false; }
        if (d.SampleDesc.Count > 1) {
            D3D11_TEXTURE2D_DESC rd = sd; rd.Usage = D3D11_USAGE_DEFAULT; rd.CPUAccessFlags = 0;
            if (FAILED(dev->CreateTexture2D(&rd, NULL, resolved.out()))) { *err = "cannot allocate the DX11 resolve texture"; return false; }
        }
        return true;
    }
    bool copy_in(IDXGISwapChain* chain, int slot) override {
        Ref<ID3D11Texture2D> back;
        if (FAILED(chain->GetBuffer(0, __uuidof(ID3D11Texture2D), (void**)back.out()))) return false;
        if (resolved.p) {
            ctx->ResolveSubresource(resolved.p, 0, back.p, 0, format);
            ctx->CopyResource(staging[slot].p, resolved.p);
        } else {
            ctx->CopyResource(staging[slot].p, back.p);
        }
        return true;
    }
    MapResult map(int slot, Mapped* out) override {
        D3D11_MAPPED_SUBRESOURCE m;
        HRESULT hr = ctx->Map(staging[slot].p, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, &m);
        if (hr == DXGI_ERROR_WAS_STILL_DRAWING) return MAP_BUSY;
        if (FAILED(hr)) return MAP_FAIL;
        out->data = (const BYTE*)m.pData; out->pitch = m.RowPitch;
        return MAP_OK;
    }
    void unmap(int slot) override { ctx->Unmap(staging[slot].p, 0); }
};

class Gpu10 : public Gpu {
    Ref<ID3D10Device> dev;
    Ref<ID3D10Texture2D> staging[kSlots];
    Ref<ID3D10Texture2D> resolved;
    DXGI_FORMAT format = DXGI_FORMAT_UNKNOWN;
public:
    bool init(IDXGISwapChain* chain, std::string* err) override {
        if (FAILED(chain->GetDevice(__uuidof(ID3D10Device), (void**)dev.out()))) { *err = "swap chain is neither DX10 nor DX11"; return false; }
        Ref<ID3D10Texture2D> back;
        if (FAILED(chain->GetBuffer(0, __uuidof(ID3D10Texture2D), (void**)back.out()))) { *err = "cannot read the DX10 back buffer"; return false; }
        D3D10_TEXTURE2D_DESC d; back->GetDesc(&d);
        if (!supported_format(d.Format)) { *err = "unsupported DX10 pixel format"; return false; }
        format = d.Format; width = d.Width; height = d.Height; rgba = is_rgba(d.Format);
        D3D10_TEXTURE2D_DESC sd = d;
        sd.Usage = D3D10_USAGE_STAGING; sd.CPUAccessFlags = D3D10_CPU_ACCESS_READ; sd.BindFlags = 0; sd.MiscFlags = 0;
        sd.SampleDesc.Count = 1; sd.SampleDesc.Quality = 0; sd.MipLevels = 1; sd.ArraySize = 1;
        for (int i = 0; i < kSlots; ++i)
            if (FAILED(dev->CreateTexture2D(&sd, NULL, staging[i].out()))) { *err = "cannot allocate DX10 staging textures"; return false; }
        if (d.SampleDesc.Count > 1) {
            D3D10_TEXTURE2D_DESC rd = sd; rd.Usage = D3D10_USAGE_DEFAULT; rd.CPUAccessFlags = 0;
            if (FAILED(dev->CreateTexture2D(&rd, NULL, resolved.out()))) { *err = "cannot allocate the DX10 resolve texture"; return false; }
        }
        return true;
    }
    bool copy_in(IDXGISwapChain* chain, int slot) override {
        Ref<ID3D10Texture2D> back;
        if (FAILED(chain->GetBuffer(0, __uuidof(ID3D10Texture2D), (void**)back.out()))) return false;
        if (resolved.p) {
            dev->ResolveSubresource(resolved.p, 0, back.p, 0, format);
            dev->CopyResource(staging[slot].p, resolved.p);
        } else {
            dev->CopyResource(staging[slot].p, back.p);
        }
        return true;
    }
    MapResult map(int slot, Mapped* out) override {
        D3D10_MAPPED_TEXTURE2D m;
        HRESULT hr = staging[slot]->Map(0, D3D10_MAP_READ, D3D10_MAP_FLAG_DO_NOT_WAIT, &m);
        if (hr == DXGI_ERROR_WAS_STILL_DRAWING) return MAP_BUSY;
        if (FAILED(hr)) return MAP_FAIL;
        out->data = (const BYTE*)m.pData; out->pitch = m.RowPitch;
        return MAP_OK;
    }
    void unmap(int slot) override { staging[slot]->Unmap(0); }
};

// ---- session -------------------------------------------------------------------------------
struct FrameBuf {
    std::vector<BYTE> pix;
    int64_t t = 0;
    uint64_t id = 0;
    bool swizzled = false;
};

struct Session {
    int fps = 30;
    std::string ffmpeg, path;
    std::wstring wpath;
    // render thread
    Gpu* gpu = nullptr;
    UINT width = 0, height = 0;
    bool rgba = false;
    uint64_t seq = 0, next_read = 0, next_id = 0;
    int64_t slot_t[kSlots] = {};
    bool slot_valid[kSlots] = {};
    std::string error;                       // init failure or a note for the status JSON
    bool resized = false;
    // pool and queue (SRW protected)
    SRWLOCK queue_lock = SRWLOCK_INIT;
    std::vector<FrameBuf*> all, free_list;
    std::deque<FrameBuf*> ready;
    HANDLE ready_event = NULL;
    // ffmpeg and writer
    HANDLE writer = NULL, process = NULL, pipe = NULL;
    std::atomic<bool> stop_requested{false}, pipe_failed{false};
    std::atomic<uint64_t> lost_queue{0}, lost_gpu{0}, frames{0}, repeated{0}, dropped_cfr{0}, late{0};
    std::atomic<uint64_t> skipped{0};            // presents not copied: too close to the last copy (capture_due)
    int64_t last_copy_us = 0;                     // render thread only
    std::atomic<int64_t> t0{0};
    long exit_code = -1;
};

enum State { IDLE, ARMED, INITED, INIT_FAILED, RUNNING, STOPPING, STOPPED };
static std::atomic<int> g_state{IDLE};
static Session* g_session;
static HANDLE g_inited, g_stopped;
static volatile LONG g_busy;                 // one thread at a time drives the GPU objects
static SRWLOCK g_control = SRWLOCK_INIT;

static std::wstring widen(const std::string& s) {
    if (s.empty()) return L"";
    int n = MultiByteToWideChar(CP_UTF8, 0, s.data(), (int)s.size(), NULL, 0);
    std::wstring w(n, L'\0');
    MultiByteToWideChar(CP_UTF8, 0, s.data(), (int)s.size(), &w[0], n);
    return w;
}

static void ensure_events() {
    if (!g_inited) g_inited = CreateEventW(NULL, TRUE, FALSE, NULL);
    if (!g_stopped) g_stopped = CreateEventW(NULL, TRUE, FALSE, NULL);
}

// ---- render thread -------------------------------------------------------------------------
static void arm(Session* s, IDXGISwapChain* chain) {
    std::string err;
    Gpu* gpu = new Gpu11();
    if (!gpu->init(chain, &err)) {
        std::string first = err;
        delete gpu;
        gpu = new Gpu10();
        if (!gpu->init(chain, &err)) { delete gpu; gpu = nullptr; err = first == "not a DX11 swap chain" ? err : first; }
    }
    int expected = ARMED;
    if (!gpu) {
        s->error = err;
        g_state.compare_exchange_strong(expected, INIT_FAILED);
        SetEvent(g_inited);
        return;
    }
    s->gpu = gpu; s->width = gpu->width; s->height = gpu->height; s->rgba = gpu->rgba;
    if (!g_state.compare_exchange_strong(expected, INITED)) {   // cancelled while initialising
        delete gpu; s->gpu = nullptr;
        return;
    }
    SetEvent(g_inited);
}

static FrameBuf* take_buffer(Session* s) {
    FrameBuf* fb = nullptr;
    AcquireSRWLockExclusive(&s->queue_lock);
    if (!s->free_list.empty()) { fb = s->free_list.back(); s->free_list.pop_back(); }
    ReleaseSRWLockExclusive(&s->queue_lock);
    return fb;
}

// Copies a mapped staging frame into a pool buffer and queues it for the writer.
static void deliver(Session* s, int slot, const Mapped& m) {
    FrameBuf* fb = take_buffer(s);
    if (!fb) { ++s->lost_queue; return; }
    const size_t row = (size_t)s->width * 4;
    for (UINT y = 0; y < s->height; ++y) memcpy(&fb->pix[(size_t)y * row], m.data + (size_t)y * m.pitch, row);
    fb->t = s->slot_t[slot]; fb->id = ++s->next_id; fb->swizzled = false;
    AcquireSRWLockExclusive(&s->queue_lock);
    s->ready.push_back(fb);
    ReleaseSRWLockExclusive(&s->queue_lock);
    SetEvent(s->ready_event);
}

static void capture(Session* s, IDXGISwapChain* chain, unsigned width, unsigned height) {
    if (width != s->width || height != s->height) { s->resized = true; ++s->lost_gpu; return; }
    int64_t wall = tracecore::now_unix_us();
    if (!videocore::capture_due(wall, s->last_copy_us, s->fps)) { ++s->skipped; return; }
    s->last_copy_us = wall;
    uint64_t n = s->seq++;
    int slot = (int)(n % kSlots);
    if (s->slot_valid[slot]) {                      // defensive: the frame that lived here was never read
        s->slot_valid[slot] = false;
        ++s->lost_gpu;
        if (s->next_read + kSlots == n) ++s->next_read;
    }
    if (s->gpu->copy_in(chain, slot)) { s->slot_t[slot] = wall; s->slot_valid[slot] = true; }
    else ++s->lost_gpu;
    // Read strictly in presentation order, no earlier than two presents after the copy.
    // Frame n-3 is the last chance: the next present overwrites its slot.
    while (s->next_read + 2 <= n) {
        int rs = (int)(s->next_read % kSlots);
        if (!s->slot_valid[rs]) { ++s->next_read; continue; }
        Mapped m;
        MapResult r = s->gpu->map(rs, &m);
        if (r == MAP_OK) {
            deliver(s, rs, m);
            s->gpu->unmap(rs);
            s->slot_valid[rs] = false;
            ++s->next_read;
        } else if (r == MAP_BUSY && s->next_read + 3 > n) {
            break;                                  // the GPU is not done yet: try again next present
        } else {
            s->slot_valid[rs] = false;              // failed, or about to be overwritten
            ++s->lost_gpu;
            ++s->next_read;
        }
    }
}

void on_present(IDXGISwapChain* chain, unsigned width, unsigned height) {
    int st = g_state.load(std::memory_order_acquire);
    if (st == IDLE || st == INITED || st == INIT_FAILED || st == STOPPED) return;
    if (InterlockedCompareExchange(&g_busy, 1, 0) != 0) return;
    Session* s = g_session;
    if (s) {
        if (st == ARMED) arm(s, chain);
        else if (st == RUNNING) capture(s, chain, width, height);
        else if (st == STOPPING) {
            delete s->gpu; s->gpu = nullptr;
            g_state.store(STOPPED, std::memory_order_release);
            SetEvent(g_stopped);
        }
    }
    InterlockedExchange(&g_busy, 0);
}

// ---- ffmpeg --------------------------------------------------------------------------------
static bool write_all(HANDLE h, const BYTE* p, size_t n) {
    while (n) {
        DWORD chunk = (DWORD)std::min<size_t>(n, 1u << 20), w = 0;
        if (!WriteFile(h, p, chunk, &w, NULL) || w == 0) return false;
        p += w; n -= w;
    }
    return true;
}

static bool spawn_ffmpeg(Session* s, std::string* err) {
    std::string exe = s->ffmpeg.empty() ? "ffmpeg" : s->ffmpeg;
    std::wstring command = L"\"" + widen(exe) + L"\"" + widen(videocore::ffmpeg_args((int)s->width, (int)s->height, s->fps, s->path));
    std::wstring log_path = s->wpath + L".ffmpeg.log";
    SECURITY_ATTRIBUTES sa = { sizeof(sa), NULL, TRUE };
    HANDLE read_end = NULL, write_end = NULL;
    if (!CreatePipe(&read_end, &write_end, &sa, 1 << 22)) { *err = "CreatePipe failed"; return false; }
    SetHandleInformation(write_end, HANDLE_FLAG_INHERIT, 0);
    HANDLE log = CreateFileW(log_path.c_str(), GENERIC_WRITE, FILE_SHARE_READ, &sa, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (log == INVALID_HANDLE_VALUE) { CloseHandle(read_end); CloseHandle(write_end); *err = "cannot create the ffmpeg log next to the output"; return false; }
    // Inherit exactly stdin and the log: the client holds other inheritable handles (login ticket).
    SIZE_T size = 0;
    InitializeProcThreadAttributeList(NULL, 1, 0, &size);
    std::vector<BYTE> list_storage(size);
    LPPROC_THREAD_ATTRIBUTE_LIST list = (LPPROC_THREAD_ATTRIBUTE_LIST)list_storage.data();
    HANDLE inherit[2] = { read_end, log };
    bool list_ready = size && InitializeProcThreadAttributeList(list, 1, 0, &size);
    bool ok = list_ready &&
              UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherit, sizeof(inherit), NULL, NULL);
    PROCESS_INFORMATION pi = {};
    if (ok) {
        STARTUPINFOEXW si = {};
        si.StartupInfo.cb = sizeof(si);
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = read_end; si.StartupInfo.hStdOutput = log; si.StartupInfo.hStdError = log;
        si.lpAttributeList = list;
        std::vector<wchar_t> mutable_command(command.begin(), command.end());
        mutable_command.push_back(L'\0');
        ok = CreateProcessW(NULL, mutable_command.data(), NULL, NULL, TRUE,
                            EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW, NULL, NULL, &si.StartupInfo, &pi) != 0;
        if (!ok) {
            char text[160];
            _snprintf_s(text, sizeof(text), _TRUNCATE, "cannot start ffmpeg (error %lu); pass ffmpeg=<path to ffmpeg.exe>", GetLastError());
            *err = text;
        }
    } else {
        *err = "cannot prepare the ffmpeg handle list";
    }
    if (list_ready) DeleteProcThreadAttributeList(list);
    CloseHandle(read_end);
    CloseHandle(log);
    if (!ok) {
        CloseHandle(write_end);
        DeleteFileW(log_path.c_str());               // nothing ran, so the log is only clutter
        return false;
    }
    CloseHandle(pi.hThread);
    s->process = pi.hProcess;
    s->pipe = write_end;
    return true;
}

// ---- writer thread -------------------------------------------------------------------------
static DWORD WINAPI writer_main(LPVOID param) {
    Session* s = (Session*)param;
    videocore::CfrScheduler sched(s->fps);
    std::map<uint64_t, FrameBuf*> held;
    int64_t newest = 0;
    const size_t bytes = (size_t)s->width * s->height * 4;
    const DWORD wait_ms = (DWORD)std::max(5, 500 / s->fps);
    auto emit = [&](uint64_t id, bool repeated) {
        std::map<uint64_t, FrameBuf*>::iterator it = held.find(id);
        if (it == held.end()) return;
        FrameBuf* fb = it->second;
        if (s->rgba && !repeated && !fb->swizzled) {          // the encoder wants BGRA
            BYTE* p = fb->pix.data();
            for (size_t i = 0; i < bytes; i += 4) { BYTE t = p[i]; p[i] = p[i + 2]; p[i + 2] = t; }
            fb->swizzled = true;
        }
        if (!s->pipe_failed && !write_all(s->pipe, fb->pix.data(), bytes)) s->pipe_failed = true;
    };
    for (;;) {
        WaitForSingleObject(s->ready_event, wait_ms);
        bool finishing = s->stop_requested.load();          // read first: everything queued before it is pulled below
        std::vector<FrameBuf*> pulled;
        AcquireSRWLockExclusive(&s->queue_lock);
        while (!s->ready.empty()) { pulled.push_back(s->ready.front()); s->ready.pop_front(); }
        ReleaseSRWLockExclusive(&s->queue_lock);
        for (size_t i = 0; i < pulled.size(); ++i) {
            sched.add(pulled[i]->t, pulled[i]->id);
            held[pulled[i]->id] = pulled[i];
            if (pulled[i]->t > newest) newest = pulled[i]->t;
        }
        if (sched.started()) {
            int64_t horizon = std::max(newest, tracecore::now_unix_us() - kLatencyUs);
            if (finishing) horizon = newest + 1000000 / s->fps;   // show the last game frame once
            sched.advance(horizon, emit);
            // Buffers behind the frame on screen are no longer needed.
            std::vector<FrameBuf*> done;
            for (std::map<uint64_t, FrameBuf*>::iterator it = held.begin(); it != held.end();) {
                if (sched.has_current() && it->first < sched.current_id()) { done.push_back(it->second); it = held.erase(it); }
                else ++it;
            }
            if (!done.empty()) {
                AcquireSRWLockExclusive(&s->queue_lock);
                for (size_t i = 0; i < done.size(); ++i) s->free_list.push_back(done[i]);
                ReleaseSRWLockExclusive(&s->queue_lock);
            }
            s->t0 = sched.t0_us();
            s->frames = sched.frames_out(); s->repeated = sched.repeated();
            s->dropped_cfr = sched.dropped(); s->late = sched.late();
        }
        if (finishing) break;
    }
    // Close the pipe, let ffmpeg finish the file.
    if (s->pipe) { CloseHandle(s->pipe); s->pipe = NULL; }
    if (s->process) {
        if (WaitForSingleObject(s->process, 60000) != WAIT_OBJECT_0) TerminateProcess(s->process, 1);
        DWORD code = 1;
        GetExitCodeProcess(s->process, &code);
        s->exit_code = (long)code;
        CloseHandle(s->process); s->process = NULL;
    }
    return 0;
}

// ---- control -------------------------------------------------------------------------------
static void append_u64(std::string* s, uint64_t v) {
    char b[32];
    _snprintf_s(b, sizeof(b), _TRUNCATE, "%llu", (unsigned long long)v);
    *s += b;
}

static std::string status_json() {
    Session* s = g_session;
    int st = g_state.load();
    bool active = st == ARMED || st == INITED || st == RUNNING || st == STOPPING;
    std::string out = "{\"recording\":";
    out += active ? "true" : "false";
    if (s && active) {
        out += ",\"path\":\""; tracecore::json_escape(&out, s->path.c_str(), 4096, false); out += '"';
        char text[160];
        _snprintf_s(text, sizeof(text), _TRUNCATE, ",\"width\":%u,\"height\":%u,\"fps\":%d,\"t0_wall_us\":%lld", s->width, s->height, s->fps, (long long)s->t0.load());
        out += text;
        out += ",\"frames\":"; append_u64(&out, s->frames);
        out += ",\"repeated\":"; append_u64(&out, s->repeated);
        out += ",\"dropped\":"; append_u64(&out, s->dropped_cfr + s->skipped + s->lost_queue + s->lost_gpu);
        out += ",\"lost_queue\":"; append_u64(&out, s->lost_queue);
        out += ",\"lost_gpu\":"; append_u64(&out, s->lost_gpu);
        out += ",\"pipe_failed\":"; out += s->pipe_failed ? "true" : "false";
        out += ",\"resized\":"; out += s->resized ? "true" : "false";
    }
    out += '}';
    return out;
}

static bool wait_idle_render() {
    for (int i = 0; i < 200; ++i) { if (!g_busy) return true; Sleep(5); }
    return false;
}

static void release_session(Session* s) {
    for (size_t i = 0; i < s->all.size(); ++i) delete s->all[i];
    if (s->ready_event) CloseHandle(s->ready_event);
    if (s->writer) CloseHandle(s->writer);
    delete s;
}

static std::string start(const std::string& args) {
    if (g_state.load() != IDLE) return "2video is already recording; stop it first";
    // fps \n ffmpeg \n path
    size_t a = args.find('\n');
    size_t b = a == std::string::npos ? a : args.find('\n', a + 1);
    if (b == std::string::npos) return "2video start expects fps, ffmpeg and path lines";
    int fps = atoi(args.substr(0, a).c_str());
    std::string ffmpeg = args.substr(a + 1, b - a - 1), path = args.substr(b + 1);
    if (fps < 1 || fps > 120) return "2fps must be 1..120";
    if (path.empty() || path.find('"') != std::string::npos || path.find('\n') != std::string::npos) return "2invalid output path";
    if (ffmpeg.find('"') != std::string::npos) return "2invalid ffmpeg path";
    if (!visuals::capture_ready()) return "2DXGI hook unavailable (use the DX10/DX11 renderer)";
    ensure_events();
    Session* s = new Session();
    s->fps = fps; s->ffmpeg = ffmpeg; s->path = path; s->wpath = widen(path);
    s->ready_event = CreateEventW(NULL, FALSE, FALSE, NULL);
    ResetEvent(g_inited);
    ResetEvent(g_stopped);
    g_session = s;
    g_state.store(ARMED, std::memory_order_release);
    if (WaitForSingleObject(g_inited, 10000) != WAIT_OBJECT_0) {
        int expected = ARMED;
        if (g_state.compare_exchange_strong(expected, IDLE)) {      // no frame was presented
            wait_idle_render();
            g_session = nullptr;
            release_session(s);
            return "2no rendered frame in 10 s (minimised, loading, or DX9 renderer)";
        }
        WaitForSingleObject(g_inited, 5000);                        // initialisation was under way
    }
    if (g_state.load() != INITED) {
        std::string why = s->error.empty() ? "video initialisation failed" : s->error;
        wait_idle_render();
        g_state.store(IDLE);
        g_session = nullptr;
        release_session(s);
        return "2" + why;
    }
    std::string err;
    const size_t bytes = (size_t)s->width * s->height * 4;
    for (int i = 0; i < kPool; ++i) {
        FrameBuf* fb = new FrameBuf();
        fb->pix.resize(bytes);
        s->all.push_back(fb); s->free_list.push_back(fb);
    }
    if (!spawn_ffmpeg(s, &err)) {
        g_state.store(STOPPING, std::memory_order_release);
        if (WaitForSingleObject(g_stopped, 3000) != WAIT_OBJECT_0 && wait_idle_render()) { delete s->gpu; s->gpu = nullptr; }
        g_state.store(IDLE);
        g_session = nullptr;
        if (!s->gpu) release_session(s);
        return "2" + err;
    }
    s->writer = CreateThread(NULL, 0, writer_main, s, 0, NULL);
    if (!s->writer) {
        CloseHandle(s->pipe); s->pipe = NULL;
        TerminateProcess(s->process, 1); CloseHandle(s->process); s->process = NULL;
        g_state.store(STOPPING, std::memory_order_release);
        WaitForSingleObject(g_stopped, 3000);
        g_state.store(IDLE);
        g_session = nullptr;
        release_session(s);
        return "2cannot start the video writer thread";
    }
    g_state.store(RUNNING, std::memory_order_release);
    return "0" + status_json();
}

static std::string stop() {
    if (g_state.load() != RUNNING) return "2video is not recording";
    Session* s = g_session;
    ResetEvent(g_stopped);
    g_state.store(STOPPING, std::memory_order_release);
    bool acked = WaitForSingleObject(g_stopped, 3000) == WAIT_OBJECT_0;   // the render thread released the GPU objects
    if (!acked && wait_idle_render()) { delete s->gpu; s->gpu = nullptr; acked = true; }   // no frames are being presented
    s->stop_requested = true;
    SetEvent(s->ready_event);
    bool writer_done = WaitForSingleObject(s->writer, 150000) == WAIT_OBJECT_0;
    int out_w = 0, out_h = 0;
    videocore::out_size((int)s->width, (int)s->height, &out_w, &out_h);   // the file's picture, not the back buffer
    std::string meta = videocore::meta_json(s->t0, s->fps, out_w, out_h, s->frames, s->repeated,
                                            s->dropped_cfr + s->skipped, s->lost_queue, s->lost_gpu, s->late, s->exit_code);
    std::wstring json_path = s->wpath + L".json";
    HANDLE f = CreateFileW(json_path.c_str(), GENERIC_WRITE, FILE_SHARE_READ, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    std::string note;
    if (f != INVALID_HANDLE_VALUE) { write_all(f, (const BYTE*)meta.data(), meta.size()); CloseHandle(f); }
    else note = ",\"sidecar\":\"cannot write the .json file\"";
    // An empty ffmpeg log is noise.
    std::wstring log_path = s->wpath + L".ffmpeg.log";
    WIN32_FILE_ATTRIBUTE_DATA info;
    if (s->exit_code == 0 && GetFileAttributesExW(log_path.c_str(), GetFileExInfoStandard, &info) && info.nFileSizeLow == 0 && !info.nFileSizeHigh)
        DeleteFileW(log_path.c_str());
    std::string reply = meta;
    reply.pop_back();
    reply += ",\"path\":\"";
    tracecore::json_escape(&reply, s->path.c_str(), 4096, false);
    reply += "\",\"pipe_failed\":";
    reply += s->pipe_failed ? "true" : "false";
    reply += ",\"resized\":";
    reply += s->resized ? "true" : "false";
    reply += note + "}";
    g_session = nullptr;
    g_state.store(IDLE, std::memory_order_release);
    if (acked && writer_done) release_session(s);   // otherwise a thread may still hold it: leak, never crash
    return "0" + reply;
}

std::string control(const std::string& command) {
    size_t split = command.find('\n');
    std::string action = command.substr(0, split);
    std::string rest = split == std::string::npos ? "" : command.substr(split + 1);
    AcquireSRWLockExclusive(&g_control);
    std::string reply;
    if (action == "start") reply = start(rest);
    else if (action == "stop") reply = stop();
    else if (action == "status") reply = "0" + status_json();
    else reply = "2video expects start, stop or status";
    ReleaseSRWLockExclusive(&g_control);
    return reply;
}

void remove() {
    if (g_state.load() == RUNNING) control("stop");
}

std::string info() { return g_state.load() == RUNNING ? " video=1" : " video=0"; }
} // namespace video
