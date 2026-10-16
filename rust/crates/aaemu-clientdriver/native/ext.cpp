// ============================================================================
//  AAEmu client extension layer (aadriver side), spec: rust/docs/client/CLIENT-EXTENSIONS.md
//
//  * gate      : x2game.dll must be the known 1.2 r208022 build (PE stamp/size plus byte
//                signatures at every address we use); anything else disables the layer and the
//                rest of the driver keeps working.
//  * send      : builds a client->server packet object (type 0x0C0) on the stack and calls the
//                client's own NetClient::Send (thiscall) from the main thread.
//  * receive   : claims one empty server->client opcode slot of the client's packet dispatcher.
//                The handler runs on the network thread, only copies the body into a bounded
//                queue; the main thread (CScriptSystem::Update hook) delivers it to Lua.
//  * lua       : natives AAEmuExt.Send/Info/Log in the UI script state plus the embedded Lua
//                bundle (native/ext/*.lua); both are re-installed when the state is rebuilt.
//
//  Every Lua C API call that can raise an error (longjmp) sits in a function without C++
//  objects that need destruction, or runs under lua_pcall.
// ============================================================================
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <new>
#include <string>
#include <deque>
#include <vector>
#include "ext.h"

namespace ext {

// ---- client build constants (x2game.dll 1.2 r208022, preferred base 0x38FF0000) -------------
static const DWORD kX2TimeDateStamp = 0x543CB835;
static const DWORD kX2SizeOfImage   = 0x01CB0A00;
static const DWORD RVA_Send         = 0x00196810;  // NetClient::Send(Packet*)  thiscall, ret 4
static const DWORD RVA_NetClientPtr = 0x00B12294;  // *(NetClient**) used as ECX
static const DWORD RVA_HdrSerialize = 0x0002E1E0;  // shared CS packet header serializer
static const DWORD RVA_PktStub      = 0x0002C160;  // xor eax,eax; ret  (packet vtable [0])
static const DWORD RVA_Dispatcher   = 0x001F6650;  // SC dispatcher
static const DWORD RVA_ScTable      = 0x00AD5728;  // handler registry (vtable ptr, slots, flags)
static const DWORD RVA_ScTableVtbl  = 0x009C2E18;  // expected value of *(void**)RVA_ScTable
static const int   kScSlots         = 0x218;
static const int   kScFlagsOffset   = 0x864;       // flag byte of opcode op: table + 0x864 + op
static const unsigned kScCandidates[] = { 0x1F2, 0x1D5, 0x159, 0x14F, 0x12A, 0x129, 0x10B, 0x104 };
static const unsigned kScKnownBusy  = 0x10D;       // SCDoodadRemoved: must be registered already
static const uint32_t kCsType       = 0x0C0;       // CSAAEmuExtPacket

static const unsigned kProto        = 1;
static const unsigned kMaxChannel   = 64;
static const unsigned kMaxPayload   = 32768;
static const size_t   kMaxQueueMsgs = 256;
static const size_t   kMaxQueueBytes = 1u << 20;
static const int      kMaxDeliverPerTick = 64;
static const DWORD    kBundleResource = 300;

// ---- Lua C API (CryScriptSystem.dll RVAs) -----------------------------------------------------
static const DWORD RVA_luaL_loadbuffer = 0x48930, RVA_lua_pcall = 0x47920, RVA_lua_tolstring = 0x46F90,
    RVA_lua_gettop = 0x46AE0, RVA_lua_settop = 0x46B00, RVA_lua_checkstack = 0x469D0,
    RVA_lua_type = 0x46CC0, RVA_lua_tonumber = 0x46ED0, RVA_lua_pushnil = 0x47160,
    RVA_lua_pushnumber = 0x47180, RVA_lua_pushlstring = 0x471D0, RVA_lua_pushstring = 0x47220,
    RVA_lua_pushcclosure = 0x472F0, RVA_lua_pushboolean = 0x47390, RVA_lua_pushlightuserdata = 0x473B0,
    RVA_lua_getfield = 0x47430, RVA_lua_createtable = 0x47510, RVA_lua_setfield = 0x47650,
    RVA_lua_touserdata = 0x470A0;
static const int LUA_GLOBALSINDEX = -10002;
enum { LUA_TNONE = -1, LUA_TNIL = 0, LUA_TBOOLEAN = 1, LUA_TLIGHTUSERDATA = 2, LUA_TNUMBER = 3,
       LUA_TSTRING = 4, LUA_TTABLE = 5, LUA_TFUNCTION = 6 };

typedef int         (__cdecl *lua_CFunction)(lua_State*);
typedef int         (__cdecl *f_loadbuffer)(lua_State*, const char*, size_t, const char*);
typedef int         (__cdecl *f_pcall)(lua_State*, int, int, int);
typedef const char* (__cdecl *f_tolstring)(lua_State*, int, size_t*);
typedef int         (__cdecl *f_gettop)(lua_State*);
typedef void        (__cdecl *f_settop)(lua_State*, int);
typedef int         (__cdecl *f_checkstack)(lua_State*, int);
typedef int         (__cdecl *f_type)(lua_State*, int);
typedef float       (__cdecl *f_tonumber)(lua_State*, int);
typedef void        (__cdecl *f_pushnil)(lua_State*);
typedef void        (__cdecl *f_pushnumber)(lua_State*, float);
typedef void        (__cdecl *f_pushlstring)(lua_State*, const char*, size_t);
typedef void        (__cdecl *f_pushstring)(lua_State*, const char*);
typedef void        (__cdecl *f_pushcclosure)(lua_State*, lua_CFunction, int);
typedef void        (__cdecl *f_pushboolean)(lua_State*, int);
typedef void        (__cdecl *f_pushlightuserdata)(lua_State*, void*);
typedef void        (__cdecl *f_getfield)(lua_State*, int, const char*);
typedef void        (__cdecl *f_createtable)(lua_State*, int, int);
typedef void        (__cdecl *f_setfield)(lua_State*, int, const char*);
typedef void*       (__cdecl *f_touserdata)(lua_State*, int);

static f_loadbuffer p_loadbuffer; static f_pcall p_pcall; static f_tolstring p_tolstring;
static f_gettop p_gettop; static f_settop p_settop; static f_checkstack p_checkstack;
static f_type p_type; static f_tonumber p_tonumber; static f_pushnil p_pushnil;
static f_pushnumber p_pushnumber; static f_pushlstring p_pushlstring; static f_pushstring p_pushstring;
static f_pushcclosure p_pushcclosure; static f_pushboolean p_pushboolean;
static f_pushlightuserdata p_pushlightuserdata; static f_getfield p_getfield;
static f_createtable p_createtable; static f_setfield p_setfield; static f_touserdata p_touserdata;

static void bindLua(BYTE* css) {
    p_loadbuffer = (f_loadbuffer)(css + RVA_luaL_loadbuffer);
    p_pcall = (f_pcall)(css + RVA_lua_pcall);
    p_tolstring = (f_tolstring)(css + RVA_lua_tolstring);
    p_gettop = (f_gettop)(css + RVA_lua_gettop);
    p_settop = (f_settop)(css + RVA_lua_settop);
    p_checkstack = (f_checkstack)(css + RVA_lua_checkstack);
    p_type = (f_type)(css + RVA_lua_type);
    p_tonumber = (f_tonumber)(css + RVA_lua_tonumber);
    p_pushnil = (f_pushnil)(css + RVA_lua_pushnil);
    p_pushnumber = (f_pushnumber)(css + RVA_lua_pushnumber);
    p_pushlstring = (f_pushlstring)(css + RVA_lua_pushlstring);
    p_pushstring = (f_pushstring)(css + RVA_lua_pushstring);
    p_pushcclosure = (f_pushcclosure)(css + RVA_lua_pushcclosure);
    p_pushboolean = (f_pushboolean)(css + RVA_lua_pushboolean);
    p_pushlightuserdata = (f_pushlightuserdata)(css + RVA_lua_pushlightuserdata);
    p_getfield = (f_getfield)(css + RVA_lua_getfield);
    p_createtable = (f_createtable)(css + RVA_lua_createtable);
    p_setfield = (f_setfield)(css + RVA_lua_setfield);
    p_touserdata = (f_touserdata)(css + RVA_lua_touserdata);
}

// ---- state ------------------------------------------------------------------------------------
struct Msg { std::string channel; std::string payload; };

static Host              g_host;
static bool              g_inited;
static int               g_gate;            // 0 unknown, 1 x2game.dll identity ok, -1 refused
static bool              g_netDone;         // gate/slot search finished (successfully or not)
static BYTE*             g_x2;              // x2game.dll base
static ULONGLONG         g_nextNet, g_waitSince;
static void*             g_sendFn;          // NetClient::Send
static volatile LONG     g_sendOk;          // Send path verified
static volatile LONG     g_recvOk;          // receive slot installed and ours
static volatile LONG     g_active;          // handler accepts messages
static unsigned          g_scOpcode;
static DWORD             g_tid;             // main thread (set by tick)
static bool              g_disabled;        // repeated faults
static int               g_faults;
static ULONGLONG         g_nextProbe, g_nextLuaTick;
static bool              g_luaReady;
static lua_State*        g_lastState;
static int               g_installFailures;
static ULONGLONG         g_retryAt;
static volatile LONG     g_sent, g_received, g_dropped, g_delivered, g_installs;

static CRITICAL_SECTION  g_qLock;
static volatile LONG     g_qInit;
static std::deque<Msg*>  g_queue;
static size_t            g_queueBytes;

struct Chunk { std::string name, text; };
static std::vector<Chunk> g_chunks;

static void xlogs(const char* s) { if (g_host.log) g_host.log(s); }
static void xlog(const char* fmt, ...) {
    char buf[600];
    va_list ap; va_start(ap, fmt);
    _vsnprintf_s(buf, sizeof(buf), _TRUNCATE, fmt, ap);
    va_end(ap);
    xlogs(buf);
}
static bool allowLog(volatile LONG* counter, LONG limit) { return InterlockedIncrement(counter) <= limit; }
static volatile LONG g_logBadBody, g_logQueueFull, g_logLuaErr, g_logSend;

static void qInit() {
    if (InterlockedCompareExchange(&g_qInit, 1, 0) == 0) { InitializeCriticalSection(&g_qLock); InterlockedExchange(&g_qInit, 2); }
    while (g_qInit != 2) Sleep(0);
}

// ---- message queue (network thread -> main thread) -------------------------------------------
static bool queuePush(const std::string& channel, const std::string& payload) {
    qInit();
    Msg* m = new (std::nothrow) Msg();
    if (!m) return false;
    m->channel = channel; m->payload = payload;
    size_t bytes = channel.size() + payload.size();
    EnterCriticalSection(&g_qLock);
    bool ok = g_queue.size() < kMaxQueueMsgs && g_queueBytes + bytes <= kMaxQueueBytes;
    if (ok) { g_queue.push_back(m); g_queueBytes += bytes; }
    LeaveCriticalSection(&g_qLock);
    if (!ok) delete m;
    return ok;
}
static Msg* queuePop() {
    qInit();
    Msg* m = NULL;
    EnterCriticalSection(&g_qLock);
    if (!g_queue.empty()) { m = g_queue.front(); g_queue.pop_front(); g_queueBytes -= m->channel.size() + m->payload.size(); }
    LeaveCriticalSection(&g_qLock);
    return m;
}
static size_t queueSize() {
    qInit();
    EnterCriticalSection(&g_qLock);
    size_t n = g_queue.size();
    LeaveCriticalSection(&g_qLock);
    return n;
}
// Puts a popped message back at the head (bounds are not re-checked: it was accepted before).
static void queueRequeueFront(Msg* m) {
    qInit();
    EnterCriticalSection(&g_qLock);
    g_queue.push_front(m); g_queueBytes += m->channel.size() + m->payload.size();
    LeaveCriticalSection(&g_qLock);
}
static void queueClear() { while (Msg* m = queuePop()) delete m; }

// ---- wire body ---------------------------------------------------------------------------------
//   u8 proto=1, u16 channel_len (1..64), channel, u32 payload_len (0..32768), payload
// Written/read through the client's own stream object, so the same code serves both directions.
struct Body { const char* channel; uint32_t channelLen; const char* payload; uint32_t payloadLen; };

static bool validChannelChar(char c) { return (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '_' || c == '.'; }
static bool validChannel(const char* s, size_t n) {
    if (n < 1 || n > kMaxChannel) return false;
    for (size_t i = 0; i < n; ++i) if (!validChannelChar(s[i])) return false;
    return true;
}

typedef void (__thiscall *StreamIO_t)(void* self, const char* name, void* val, int extra);
typedef char (__thiscall *StreamFlag_t)(void* self);
static void** streamVtbl(void* s) { return *(void***)s; }
static bool sValid(void* s) { return ((StreamFlag_t)streamVtbl(s)[0x28 / 4])(s) != 0; }
static void sU8(void* s, const char* n, uint8_t* v)   { ((StreamIO_t)streamVtbl(s)[0x44 / 4])(s, n, v, 0); }
static void sU16(void* s, const char* n, uint16_t* v) { ((StreamIO_t)streamVtbl(s)[0x40 / 4])(s, n, v, 0); }
static void sU32(void* s, const char* n, uint32_t* v) { ((StreamIO_t)streamVtbl(s)[0x3C / 4])(s, n, v, 0); }

static void writeBody(void* stream, const Body& b) {
    uint8_t proto = (uint8_t)kProto;
    uint16_t cl = (uint16_t)b.channelLen;
    uint32_t pl = b.payloadLen;
    sU8(stream, "proto", &proto);
    sU16(stream, "channel_len", &cl);
    for (uint32_t i = 0; i < b.channelLen; ++i) { uint8_t c = (uint8_t)b.channel[i]; sU8(stream, "channel", &c); }
    sU32(stream, "payload_len", &pl);
    for (uint32_t i = 0; i < b.payloadLen; ++i) { uint8_t c = (uint8_t)b.payload[i]; sU8(stream, "payload", &c); }
}

enum ReadResult { RR_OK, RR_BAD_PROTO, RR_BAD_CHANNEL, RR_TOO_BIG, RR_STREAM };

static ReadResult readBody(void* stream, std::string& channel, std::string& payload) {
    uint8_t proto = 0; uint16_t cl = 0; uint32_t pl = 0;
    sU8(stream, "proto", &proto);
    sU16(stream, "channel_len", &cl);
    if (!sValid(stream)) return RR_STREAM;
    if (proto != kProto) return RR_BAD_PROTO;
    if (cl < 1 || cl > kMaxChannel) return RR_BAD_CHANNEL;
    channel.assign(cl, '\0');
    for (uint16_t i = 0; i < cl; ++i) { uint8_t c = 0; sU8(stream, "channel", &c); channel[i] = (char)c; }
    sU32(stream, "payload_len", &pl);
    if (!sValid(stream)) return RR_STREAM;
    if (!validChannel(channel.data(), channel.size())) return RR_BAD_CHANNEL;
    if (pl > kMaxPayload) return RR_TOO_BIG;
    payload.assign(pl, '\0');
    for (uint32_t i = 0; i < pl; ++i) {
        uint8_t c = 0; sU8(stream, "payload", &c); payload[i] = (char)c;
        if ((i & 1023) == 1023 && !sValid(stream)) return RR_STREAM;
    }
    return sValid(stream) ? RR_OK : RR_STREAM;
}

// Byte image of the body (documentation / tests; the server decodes the same layout).
std::string encodeBodyBytes(const std::string& channel, const std::string& payload) {
    std::string out;
    out.push_back((char)kProto);
    out.push_back((char)(channel.size() & 0xFF)); out.push_back((char)(channel.size() >> 8));
    out += channel;
    uint32_t n = (uint32_t)payload.size();
    for (int i = 0; i < 4; ++i) out.push_back((char)((n >> (8 * i)) & 0xFF));
    out += payload;
    return out;
}

// ---- send path ---------------------------------------------------------------------------------
struct ExtPacket {                 // layout shared by every 1.2 CS packet object
    void**       vtbl;             // +0
    uint32_t     type;             // +4
    uint8_t      count;            // +8
    uint8_t      hash;             // +9
    uint8_t      pad[2];           // +A
    const Body*  body;             // +C (first payload field of stock packets)
    uint32_t     spare[4];         // slack: stock packets are larger
};
static_assert(offsetof(ExtPacket, type) == 4 && offsetof(ExtPacket, count) == 8 &&
              offsetof(ExtPacket, hash) == 9 && offsetof(ExtPacket, body) == 0xC, "CS packet layout");

static void* g_pktVtbl[6];

// vtbl[2]: thiscall(this, Stream*), callee pops the stream argument.
static void __fastcall pktBodySerializer(ExtPacket* self, void* /*edx*/, void* stream) {
    if (self->body) writeBody(stream, *self->body);
}

typedef void (__thiscall *Send_t)(void* netClient, void* packet);

static void buildPacketVtable(BYTE* x2) {
    void* stub = x2 + RVA_PktStub;
    void* hdr = x2 + RVA_HdrSerialize;
    // Stock packet classes carry two (stub, header, body) triples; mirror that.
    g_pktVtbl[0] = stub; g_pktVtbl[1] = hdr; g_pktVtbl[2] = (void*)&pktBodySerializer;
    g_pktVtbl[3] = stub; g_pktVtbl[4] = hdr; g_pktVtbl[5] = (void*)&pktBodySerializer;
}

static const char* sendRawSeh(void* netClient, ExtPacket* pkt) {
    __try { ((Send_t)g_sendFn)(netClient, pkt); }
    __except (EXCEPTION_EXECUTE_HANDLER) { return "exception inside NetClient::Send"; }
    return NULL;
}

// Returns NULL on success, otherwise a static error text. Main thread only.
static const char* sendPacket(const char* channel, size_t cl, const char* payload, size_t pl) {
    if (!g_sendOk) return "extension send path is not available";
    if (g_tid && GetCurrentThreadId() != g_tid) return "AAEmuExt.Send must run on the main thread";
    if (!validChannel(channel, cl)) return "invalid channel (1..64 chars of [a-z0-9_.])";
    if (pl > kMaxPayload) return "payload too large (max 32768 bytes)";
    void* nc = *(void**)(g_x2 + RVA_NetClientPtr);
    if (!nc) return "not connected to a game server";
    Body body = { channel, (uint32_t)cl, payload, (uint32_t)pl };
    ExtPacket pkt;
    memset(&pkt, 0, sizeof(pkt));
    pkt.vtbl = g_pktVtbl; pkt.type = kCsType; pkt.body = &body;
    const char* err = sendRawSeh(nc, &pkt);
    if (!err) {
        InterlockedIncrement(&g_sent);
        if (allowLog(&g_logSend, 3)) xlog("ext: sent %.*s (%u bytes)", (int)cl, channel, (unsigned)pl);
    }
    return err;
}

// ---- receive path -------------------------------------------------------------------------------
// Handler object stored in the dispatcher's table: { vtbl }. Slot indices follow the stock
// handlers: [0] deleting destructor, [1] build main-thread job (flag 0), [2] run directly (flag 1).
static void* g_hdlVtbl[3];
static struct { void** vtbl; } g_hdlObj;

static void* __fastcall hdlDtor(void* self, void* /*edx*/, unsigned char /*flags*/) { return self; }  // never freed
static void* __fastcall hdlJob(void* /*self*/, void* /*edx*/, void* /*reader*/, int /*op*/) { return NULL; }

static int recvImpl(void* stream) {
    try {
        std::string channel, payload;
        ReadResult r = readBody(stream, channel, payload);
        if (r == RR_STREAM) { InterlockedIncrement(&g_dropped); return 0; }
        if (r != RR_OK) {
            InterlockedIncrement(&g_dropped);
            if (allowLog(&g_logBadBody, 20)) xlog("ext: dropped malformed message (reason %d)", (int)r);
            return 1;
        }
        if (!g_active) { InterlockedIncrement(&g_dropped); return 1; }
        if (queuePush(channel, payload)) InterlockedIncrement(&g_received);
        else {
            InterlockedIncrement(&g_dropped);
            if (allowLog(&g_logQueueFull, 20)) xlog("ext: receive queue full, dropped %s", channel.c_str());
        }
        return 1;
    } catch (...) {
        InterlockedIncrement(&g_dropped);
        return 1;
    }
}
static int recvSeh(void* stream) {
    __try { return recvImpl(stream); }
    __except (EXCEPTION_EXECUTE_HANDLER) { InterlockedIncrement(&g_dropped); return 0; }
}
// vtbl[2](reader, op, owner), thiscall, callee pops 12 bytes. Returns nonzero = handled.
static int __fastcall hdlRecv(void* /*self*/, void* /*edx*/, void* stream, int /*op*/, void* /*owner*/) {
    return recvSeh(stream);
}

static void initHandlerObject() {
    g_hdlVtbl[0] = (void*)&hdlDtor; g_hdlVtbl[1] = (void*)&hdlJob; g_hdlVtbl[2] = (void*)&hdlRecv;
    g_hdlObj.vtbl = g_hdlVtbl;
}

static void** scSlot(BYTE* x2, unsigned op) { return (void**)(x2 + RVA_ScTable + 4 + 4 * op); }
static BYTE*  scFlag(BYTE* x2, unsigned op) { return x2 + RVA_ScTable + kScFlagsOffset + op; }

// The stock registration (528 handlers) has run: right vtable and a densely filled table.
static bool registryReady(BYTE* x2) {
    if (*(void**)(x2 + RVA_ScTable) != (void*)(x2 + RVA_ScTableVtbl)) return false;
    if (!*scSlot(x2, kScKnownBusy)) return false;
    int filled = 0;
    for (int op = 0; op < kScSlots; ++op) if (*scSlot(x2, (unsigned)op)) ++filled;
    return filled >= 400;
}
static int pickOpcode(BYTE* x2) {
    for (size_t i = 0; i < sizeof(kScCandidates) / sizeof(kScCandidates[0]); ++i)
        if (!*scSlot(x2, kScCandidates[i])) return (int)kScCandidates[i];
    return -1;
}
// Flag first, then the slot: the dispatcher only looks at the flag after finding a handler.
static void installHandler(BYTE* x2, unsigned op) {
    initHandlerObject();
    g_scOpcode = op;
    InterlockedExchange(&g_active, 1);
    *scFlag(x2, op) = 1;
    InterlockedExchangePointer((volatile PVOID*)scSlot(x2, op), &g_hdlObj);
    InterlockedExchange(&g_recvOk, 1);
}
static void restoreHandler(BYTE* x2) {
    InterlockedExchange(&g_active, 0);
    if (g_recvOk && x2) {
        if (*scSlot(x2, g_scOpcode) == (void*)&g_hdlObj) {
            InterlockedExchangePointer((volatile PVOID*)scSlot(x2, g_scOpcode), NULL);
            *scFlag(x2, g_scOpcode) = 0;
        }
    }
    InterlockedExchange(&g_recvOk, 0);
}

// ---- x2game.dll gate -----------------------------------------------------------------------------
enum Identity { ID_OK, ID_MISMATCH };

static Identity identitySeh(const BYTE* b, DWORD* stamp, DWORD* size) {
    __try {
        if (((const IMAGE_DOS_HEADER*)b)->e_magic != IMAGE_DOS_SIGNATURE) return ID_MISMATCH;
        const IMAGE_NT_HEADERS* nt = (const IMAGE_NT_HEADERS*)(b + ((const IMAGE_DOS_HEADER*)b)->e_lfanew);
        if (nt->Signature != IMAGE_NT_SIGNATURE) return ID_MISMATCH;
        *stamp = nt->FileHeader.TimeDateStamp; *size = nt->OptionalHeader.SizeOfImage;
    } __except (EXCEPTION_EXECUTE_HANDLER) { return ID_MISMATCH; }
    return (*stamp == kX2TimeDateStamp && *size == kX2SizeOfImage) ? ID_OK : ID_MISMATCH;
}

// Code bytes we depend on, none containing relocated addresses. The dump is unpacked at run time;
// while the loader has not decrypted .text yet these do not match and the caller keeps waiting.
static bool signaturesSeh(const BYTE* b) {
    static const BYTE sSend[]  = { 0xE9, 0x77, 0x9D, 0xA8, 0x01 };                 // jmp into the packed section
    static const BYTE sHdr[]   = { 0x55, 0x8B, 0xEC, 0x51, 0x56, 0x8B, 0xF1, 0x8B, 0x4D, 0x08, 0x8B, 0x01, 0x8B, 0x40, 0x44 };
    static const BYTE sStub[]  = { 0x33, 0xC0, 0xC3 };
    static const BYTE sDisp[]  = { 0x55, 0x8B, 0xEC, 0x6A, 0xFF };
    __try {
        return memcmp(b + RVA_Send, sSend, sizeof(sSend)) == 0 && memcmp(b + RVA_HdrSerialize, sHdr, sizeof(sHdr)) == 0 &&
               memcmp(b + RVA_PktStub, sStub, sizeof(sStub)) == 0 && memcmp(b + RVA_Dispatcher, sDisp, sizeof(sDisp)) == 0;
    } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}
static bool registrySeh(BYTE* b) {
    __try { return registryReady(b); } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}

static void netStepWith(BYTE* b, ULONGLONG now) {
    if (g_gate == 0) {
        DWORD stamp = 0, size = 0;
        if (identitySeh(b, &stamp, &size) != ID_OK) {
            g_gate = -1;
            xlog("ext: x2game.dll is not the 1.2 r208022 build (stamp %08lX size %08lX) - extension disabled", stamp, size);
            return;
        }
        g_gate = 1; g_x2 = b; g_waitSince = now;
    }
    if (!signaturesSeh(b)) {
        if (now - g_waitSince > 180000) { g_netDone = true; xlog("ext: x2game.dll code does not match the known build - extension transport disabled"); }
        return;
    }
    if (!g_sendOk) {
        g_sendFn = b + RVA_Send;
        buildPacketVtable(b);
        InterlockedExchange(&g_sendOk, 1);
    }
    if (!registrySeh(b)) {
        if (now - g_waitSince > 180000) { g_netDone = true; xlog("ext: packet registry never became ready - receive disabled"); }
        return;
    }
    int op = pickOpcode(b);
    if (op < 0) { g_netDone = true; xlog("ext: no free server->client opcode slot - receive disabled"); return; }
    installHandler(b, (unsigned)op);
    g_netDone = true;
    xlog("ext: transport ready - send via x2game+%06lX, receive on SC opcode 0x%X (%u)", RVA_Send, (unsigned)op, (unsigned)op);
}

static void netStep(ULONGLONG now) {
    if (g_gate < 0 || g_netDone) return;
    if (now < g_nextNet) return;
    g_nextNet = now + 1000;
    HMODULE m = GetModuleHandleA("x2game.dll");
    if (m) netStepWith((BYTE*)m, now);
}

// ---- Lua natives (UI script state) --------------------------------------------------------------
static const char kLuaCoreMarker[] = "_core";

// AAEmuExt.Send(channel, payload) -> true | nil, err
static int __cdecl luaSend(lua_State* L) {
    const char* ch = NULL; const char* pay = NULL;
    size_t cl = 0, pl = 0;
    int t1 = p_type(L, 1), t2 = p_type(L, 2);
    if (t1 == LUA_TSTRING || t1 == LUA_TNUMBER) ch = p_tolstring(L, 1, &cl);
    if (t2 == LUA_TSTRING || t2 == LUA_TNUMBER) pay = p_tolstring(L, 2, &pl);
    else if (t2 == LUA_TNIL || t2 == LUA_TNONE) pay = "";
    const char* err;
    if (!ch) err = "channel must be a string";
    else if (!pay) err = "payload must be a string";
    else err = sendPacket(ch, cl, pay, pl);
    if (!err) { p_pushboolean(L, 1); return 1; }
    p_pushnil(L); p_pushstring(L, err);
    return 2;
}

// AAEmuExt.Info() -> { proto, driver, sc_opcode, hooked, send }
static int __cdecl luaInfo(lua_State* L) {
    p_createtable(L, 0, 5);
    p_pushnumber(L, (float)kProto);                       p_setfield(L, -2, "proto");
    p_pushstring(L, g_host.driverVersion ? g_host.driverVersion : "aadriver"); p_setfield(L, -2, "driver");
    p_pushnumber(L, (float)(g_recvOk ? g_scOpcode : 0));  p_setfield(L, -2, "sc_opcode");
    p_pushboolean(L, g_recvOk ? 1 : 0);                   p_setfield(L, -2, "hooked");
    p_pushboolean(L, g_sendOk ? 1 : 0);                   p_setfield(L, -2, "send");
    return 1;
}

// AAEmuExt.Log(text)
static int __cdecl luaLog(lua_State* L) {
    int t = p_type(L, 1);
    const char* s = (t == LUA_TSTRING || t == LUA_TNUMBER) ? p_tolstring(L, 1, NULL) : NULL;
    char buf[700];
    _snprintf_s(buf, sizeof(buf), _TRUNCATE, "lua: %s", s ? s : (t == LUA_TNIL || t == LUA_TNONE ? "nil" : "<value>"));
    xlogs(buf);
    return 0;
}

// Creates the global AAEmuExt table with the natives. Run under lua_pcall.
static int __cdecl luaRegister(lua_State* L) {
    p_createtable(L, 0, 8);
    p_pushcclosure(L, luaSend, 0); p_setfield(L, -2, "Send");
    p_pushcclosure(L, luaInfo, 0); p_setfield(L, -2, "Info");
    p_pushcclosure(L, luaLog, 0);  p_setfield(L, -2, "Log");
    p_pushnumber(L, 1.0f);         p_setfield(L, -2, "_native");
    p_setfield(L, LUA_GLOBALSINDEX, "AAEmuExt");
    return 0;
}

// Pushes bit0 = natives present, bit1 = core.lua loaded. Run under lua_pcall.
static int __cdecl luaProbe(lua_State* L) {
    int flags = 0;
    int base = p_gettop(L);
    p_getfield(L, LUA_GLOBALSINDEX, "AAEmuExt");
    if (p_type(L, -1) == LUA_TTABLE) {
        p_getfield(L, -1, "_native");
        if (p_type(L, -1) == LUA_TNUMBER) flags |= 1;
        p_settop(L, -2);
        p_getfield(L, -1, kLuaCoreMarker);
        if (p_type(L, -1) == LUA_TSTRING) flags |= 2;
        p_settop(L, -2);
    }
    p_settop(L, base);
    p_getfield(L, LUA_GLOBALSINDEX, "AAEmuExt_Receive");
    if (p_type(L, -1) != LUA_TFUNCTION) flags &= ~2;
    p_settop(L, base);
    p_pushnumber(L, (float)flags);
    return 1;
}

static void logLuaError(lua_State* L, const char* what) {
    size_t n = 0;
    const char* e = p_tolstring(L, -1, &n);
    if (allowLog(&g_logLuaErr, 30)) xlog("ext: %s: %s", what, e ? e : "?");
}

// AAEmuExt_Receive(channel, payload) for the Msg passed as light userdata. Run under lua_pcall.
// Returns 1 when the message was handed to Lua, 0 when AAEmuExt_Receive does not exist (yet).
static int __cdecl luaDeliver(lua_State* L) {
    const Msg* m = (const Msg*)p_touserdata(L, 1);
    p_getfield(L, LUA_GLOBALSINDEX, "AAEmuExt_Receive");
    if (p_type(L, -1) != LUA_TFUNCTION) { p_pushnumber(L, 0.0f); return 1; }
    p_pushlstring(L, m->channel.data(), m->channel.size());
    p_pushlstring(L, m->payload.data(), m->payload.size());
    if (p_pcall(L, 2, 0, 0) != 0) { logLuaError(L, "AAEmuExt_Receive failed"); p_settop(L, 1); }
    p_pushnumber(L, 1.0f);
    return 1;
}

// AAEmuExt_Tick() if defined. Run under lua_pcall.
static int __cdecl luaTick(lua_State* L) {
    p_getfield(L, LUA_GLOBALSINDEX, "AAEmuExt_Tick");
    if (p_type(L, -1) != LUA_TFUNCTION) return 0;
    if (p_pcall(L, 0, 0, 0) != 0) logLuaError(L, "AAEmuExt_Tick failed");
    return 0;
}

static int protectedCall(lua_State* L, lua_CFunction fn, void* arg) {
    int top = p_gettop(L);
    int rc = -1;
    if (p_checkstack(L, 10)) {
        p_pushcclosure(L, fn, 0);
        int nargs = 0;
        if (arg) { p_pushlightuserdata(L, arg); nargs = 1; }
        rc = p_pcall(L, nargs, 0, 0);
        if (rc != 0) logLuaError(L, "protected call failed");
    }
    p_settop(L, top);
    return rc;
}

static int probe(lua_State* L) {
    int top = p_gettop(L);
    int flags = 0;
    if (p_checkstack(L, 10)) {
        p_pushcclosure(L, luaProbe, 0);
        if (p_pcall(L, 0, 1, 0) == 0 && p_type(L, -1) == LUA_TNUMBER) flags = (int)p_tonumber(L, -1);
    }
    p_settop(L, top);
    return flags;
}

// Runs one embedded chunk. On failure `err` receives the message.
static bool runChunk(lua_State* L, const char* name, const char* text, size_t len, char* err, size_t errSize) {
    int top = p_gettop(L);
    char chunkName[128];
    _snprintf_s(chunkName, sizeof(chunkName), _TRUNCATE, "=ext/%s", name);
    int rc = p_loadbuffer(L, text, len, chunkName);
    if (rc == 0) rc = p_pcall(L, 0, 0, 0);
    if (rc != 0) {
        size_t n = 0; const char* e = p_tolstring(L, -1, &n);
        _snprintf_s(err, errSize, _TRUNCATE, "%s", e ? e : "?");
    }
    p_settop(L, top);
    return rc == 0;
}

// ---- bundle -------------------------------------------------------------------------------------
// Sections start with a line "--@@ext chunk <name>" (a Lua comment, so the bundle is valid Lua).
static std::vector<Chunk> splitBundle(const char* data, size_t size) {
    static const char marker[] = "--@@ext chunk ";
    const size_t mlen = sizeof(marker) - 1;
    std::vector<Chunk> out;
    size_t pos = 0;
    while (pos < size) {
        size_t eol = pos;
        while (eol < size && data[eol] != '\n') ++eol;
        size_t lineEnd = eol;
        if (lineEnd > pos && data[lineEnd - 1] == '\r') --lineEnd;
        if (lineEnd - pos >= mlen && memcmp(data + pos, marker, mlen) == 0) {
            Chunk c;
            c.name.assign(data + pos + mlen, lineEnd - pos - mlen);
            out.push_back(c);
        } else if (!out.empty()) {
            out.back().text.append(data + pos, (eol < size ? eol + 1 : eol) - pos);
        }
        pos = eol < size ? eol + 1 : eol;
    }
    return out;
}

// ---- (re)installation into the UI state ------------------------------------------------------------
static bool installLua(lua_State* L, int have) {
    if (!(have & 1)) {
        if (protectedCall(L, luaRegister, NULL) != 0) return false;
    }
    char err[512];
    bool coreOk = false;
    int modulesOk = 0, modulesFailed = 0;
    for (size_t i = 0; i < g_chunks.size(); ++i) {
        const Chunk& c = g_chunks[i];
        bool ok = runChunk(L, c.name.c_str(), c.text.data(), c.text.size(), err, sizeof(err));
        if (i == 0) {
            coreOk = ok;
            if (!ok) { xlog("ext: %s failed: %s", c.name.c_str(), err); return false; }
        } else if (ok) ++modulesOk;
        else { ++modulesFailed; xlog("ext: module %s failed: %s", c.name.c_str(), err); }
    }
    InterlockedIncrement(&g_installs);
    xlog("ext: Lua installed in UI state %p (%d module chunk(s) ok, %d failed)", L, modulesOk, modulesFailed);
    return coreOk;
}

// Hands one message to Lua. False when the Lua side is missing (state just rebuilt).
static bool deliverOne(lua_State* L, Msg* m) {
    int top = p_gettop(L);
    bool delivered = false;
    if (p_checkstack(L, 10)) {
        p_pushcclosure(L, luaDeliver, 0);
        p_pushlightuserdata(L, m);
        if (p_pcall(L, 1, 1, 0) == 0) delivered = p_type(L, -1) == LUA_TNUMBER && p_tonumber(L, -1) != 0.0f;
        else logLuaError(L, "delivery failed");
    }
    p_settop(L, top);
    return delivered;
}

static void deliverAll(lua_State* L) {
    for (int n = 0; n < kMaxDeliverPerTick; ++n) {
        Msg* m = queuePop();
        if (!m) break;
        if (!deliverOne(L, m)) {
            // AAEmuExt_Receive is gone: keep the message, force a re-probe/re-install.
            queueRequeueFront(m);
            g_luaReady = false; g_nextProbe = 0;
            return;
        }
        InterlockedIncrement(&g_delivered);
        delete m;
    }
}

static void tickImpl(lua_State* L) {
    ULONGLONG now = GetTickCount64();
    if (!g_tid) g_tid = GetCurrentThreadId();
    netStep(now);
    if (g_gate <= 0 || !L) return;  // x2game.dll refused or not seen yet: leave the UI state alone
    if (L != g_lastState) { g_lastState = L; g_nextProbe = 0; g_luaReady = false; }
    if (now >= g_nextProbe && now >= g_retryAt) {
        g_nextProbe = now + 250;
        int flags = probe(L);
        if (flags != 3) {
            g_luaReady = false;
            if (g_chunks.empty()) { g_retryAt = now + 60000; return; }
            if (installLua(L, flags) && probe(L) == 3) { g_luaReady = true; g_installFailures = 0; }
            else if (++g_installFailures >= 5) { g_retryAt = now + 30000; g_installFailures = 0; xlog("ext: Lua install keeps failing, pausing 30 s"); }
        } else g_luaReady = true;
        // Someone replaced our dispatcher slot (should never happen): report and stop receiving.
        if (g_recvOk && *scSlot(g_x2, g_scOpcode) != (void*)&g_hdlObj) {
            InterlockedExchange(&g_recvOk, 0); InterlockedExchange(&g_active, 0);
            xlog("ext: receive slot 0x%X was overwritten by the client - receive disabled", g_scOpcode);
        }
    }
    if (!g_luaReady) return;
    if (queueSize()) deliverAll(L);
    if (now >= g_nextLuaTick) { g_nextLuaTick = now + 500; protectedCall(L, luaTick, NULL); }
}

// ---- public API -----------------------------------------------------------------------------------
void init(const Host& host) {
    g_host = host;
    qInit();
    if (host.css) bindLua(host.css);
    g_chunks = splitBundle(host.bundle ? host.bundle : "", host.bundle ? host.bundleSize : 0);
    g_inited = true;
    xlog("ext: layer ready (%u embedded Lua chunk(s)); waiting for x2game.dll and the UI script state", (unsigned)g_chunks.size());
}

void tick(lua_State* ui) {
    if (!g_inited || g_disabled) return;
    __try { tickImpl(ui); }
    __except (EXCEPTION_EXECUTE_HANDLER) {
        if (++g_faults >= 5) { g_disabled = true; restoreHandler(g_x2); xlog("ext: repeated native faults - extension disabled"); }
        else xlog("ext: native fault in tick (%d)", g_faults);
    }
}

void remove() {
    g_disabled = true;
    InterlockedExchange(&g_sendOk, 0);
    restoreHandler(g_x2);
    queueClear();
}

std::string info() {
    char buf[256];
    _snprintf_s(buf, sizeof(buf), _TRUNCATE,
                " ext=gate:%d,send:%ld,recv:%ld,op:0x%X,lua:%d,installs:%ld,sent:%ld,rx:%ld,deliv:%ld,drop:%ld",
                g_gate, g_sendOk, g_recvOk, g_recvOk ? g_scOpcode : 0, g_luaReady ? 1 : 0,
                g_installs, g_sent, g_received, g_delivered, g_dropped);
    return buf;
}

}  // namespace ext
