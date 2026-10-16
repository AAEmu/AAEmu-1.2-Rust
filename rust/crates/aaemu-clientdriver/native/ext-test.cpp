// Unit tests for the client extension layer: wire body, queue, x2game gate, receive-slot claim and
// the full dispatcher/Send call conventions against a synthetic image and a fake stream. No game.
#include "ext.cpp"
#include <stdio.h>
#include <stdlib.h>

using namespace ext;

static int g_failures;
#define CHECK(cond) do { if (!(cond)) { fprintf(stderr, "ext test failed: %s (line %d)\n", #cond, __LINE__); ++g_failures; } } while (0)

static void testLog(const char* line) { printf("  [log] %s\n", line); }

// ---- fake client stream (same vtable slots/conventions as the engine's) --------------------------------
struct FakeStream {
    void**      vtbl;
    bool        reading;
    bool        valid;
    std::string bytes;
    size_t      pos;
    std::vector<std::string> names;
};
static char __fastcall fIsReading(FakeStream* s, void*) { return s->reading ? 1 : 0; }
static char __fastcall fIsValid(FakeStream* s, void*) { return s->valid ? 1 : 0; }
static void ioBytes(FakeStream* s, const char* name, void* v, int n) {
    s->names.push_back(name);
    if (s->reading) {
        uint32_t out = 0;
        for (int i = 0; i < n; ++i) {
            if (s->pos >= s->bytes.size()) { s->valid = false; out = 0; break; }
            out |= (uint32_t)(uint8_t)s->bytes[s->pos++] << (8 * i);
        }
        memcpy(v, &out, n);
    } else {
        s->bytes.append((const char*)v, n);
    }
}
static void __fastcall fU8(FakeStream* s, void*, const char* n, void* v, int) { ioBytes(s, n, v, 1); }
static void __fastcall fU16(FakeStream* s, void*, const char* n, void* v, int) { ioBytes(s, n, v, 2); }
static void __fastcall fU32(FakeStream* s, void*, const char* n, void* v, int) { ioBytes(s, n, v, 4); }
static void* g_fakeVtbl[24];

static FakeStream makeStream(bool reading, const std::string& bytes = std::string()) {
    g_fakeVtbl[0x1C / 4] = (void*)&fIsReading;
    g_fakeVtbl[0x28 / 4] = (void*)&fIsValid;
    g_fakeVtbl[0x3C / 4] = (void*)&fU32;
    g_fakeVtbl[0x40 / 4] = (void*)&fU16;
    g_fakeVtbl[0x44 / 4] = (void*)&fU8;
    FakeStream s; s.vtbl = g_fakeVtbl; s.reading = reading; s.valid = true; s.bytes = bytes; s.pos = 0;
    return s;
}

static std::string hex(const std::string& s) {
    std::string out; char b[4];
    for (size_t i = 0; i < s.size(); ++i) { _snprintf_s(b, sizeof(b), _TRUNCATE, "%02x", (uint8_t)s[i]); out += b; }
    return out;
}

// ---- synthetic x2game image --------------------------------------------------------------------------
static const DWORD kImageSize = 0x1000000;  // covers every RVA we touch (highest 0xB12298)
static BYTE* makeImage(bool withSignatures, bool withRegistry) {
    BYTE* img = (BYTE*)VirtualAlloc(NULL, kImageSize, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    CHECK(img != NULL);
    IMAGE_DOS_HEADER* dos = (IMAGE_DOS_HEADER*)img;
    dos->e_magic = IMAGE_DOS_SIGNATURE; dos->e_lfanew = 0x80;
    IMAGE_NT_HEADERS* nt = (IMAGE_NT_HEADERS*)(img + 0x80);
    nt->Signature = IMAGE_NT_SIGNATURE;
    nt->FileHeader.TimeDateStamp = kX2TimeDateStamp;
    nt->OptionalHeader.SizeOfImage = kX2SizeOfImage;
    if (withSignatures) {
        static const BYTE sSend[] = { 0xE9, 0x77, 0x9D, 0xA8, 0x01 };
        static const BYTE sHdr[] = { 0x55, 0x8B, 0xEC, 0x51, 0x56, 0x8B, 0xF1, 0x8B, 0x4D, 0x08, 0x8B, 0x01, 0x8B, 0x40, 0x44 };
        static const BYTE sStub[] = { 0x33, 0xC0, 0xC3 };
        static const BYTE sDisp[] = { 0x55, 0x8B, 0xEC, 0x6A, 0xFF };
        memcpy(img + RVA_Send, sSend, sizeof(sSend)); memcpy(img + RVA_HdrSerialize, sHdr, sizeof(sHdr));
        memcpy(img + RVA_PktStub, sStub, sizeof(sStub)); memcpy(img + RVA_Dispatcher, sDisp, sizeof(sDisp));
    }
    if (withRegistry) {
        *(void**)(img + RVA_ScTable) = img + RVA_ScTableVtbl;
        for (int op = 0; op < kScSlots; ++op) {
            bool candidate = false;
            for (size_t i = 0; i < sizeof(kScCandidates) / sizeof(kScCandidates[0]); ++i) candidate |= kScCandidates[i] == (unsigned)op;
            if (!candidate) *scSlot(img, (unsigned)op) = (void*)(uintptr_t)(0x1000 + op);  // some stock handler
        }
    }
    return img;
}

static void resetState() {
    restoreHandler(g_x2);
    queueClear();
    g_gate = 0; g_netDone = false; g_x2 = NULL; g_nextNet = 0; g_waitSince = 0; g_sendFn = NULL;
    g_sendOk = 0; g_recvOk = 0; g_active = 0; g_scOpcode = 0; g_tid = 0;
    g_sent = g_received = g_dropped = 0;
    g_logBadBody = g_logQueueFull = g_logSend = 0;
}

// Mirrors the stock dispatcher's flag-1 path: handler->vtbl[2](reader, op, owner), thiscall.
typedef int (__thiscall *Handler2_t)(void* self, void* reader, int op, void* owner);
static int dispatch(BYTE* img, FakeStream* st, unsigned op) {
    void* h = *scSlot(img, op);
    if (!h) return -1;
    if (!*scFlag(img, op)) return -2;
    void** vt = *(void***)h;
    return ((Handler2_t)vt[2])(h, st, (int)op, (void*)0x1234);
}

// ---- tests -------------------------------------------------------------------------------------------------
static void testBodyEncodeDecode() {
    const std::string channel = "ext.debug.ping", payload = "{\"n\":7}";
    FakeStream w = makeStream(false);
    Body b = { channel.data(), (uint32_t)channel.size(), payload.data(), (uint32_t)payload.size() };
    writeBody(&w, b);
    CHECK(w.bytes == encodeBodyBytes(channel, payload));
    // Exact layout: proto 1, u16 14, "ext.debug.ping", u32 7, payload.
    CHECK(hex(w.bytes) == "010e00" + hex(channel) + "07000000" + hex(payload));
    CHECK(w.names.size() == 1 + 1 + channel.size() + 1 + payload.size());
    CHECK(w.names[0] == "proto" && w.names[1] == "channel_len" && w.names[2] == "channel");

    FakeStream r = makeStream(true, w.bytes);
    std::string c, p;
    CHECK(readBody(&r, c, p) == RR_OK && c == channel && p == payload && r.pos == w.bytes.size());

    // Empty payload, maximum sizes.
    std::string maxChannel(64, 'a'), maxPayload(kMaxPayload, 'x');
    FakeStream r2 = makeStream(true, encodeBodyBytes(maxChannel, maxPayload));
    CHECK(readBody(&r2, c, p) == RR_OK && c == maxChannel && p == maxPayload);
    FakeStream r3 = makeStream(true, encodeBodyBytes("a", ""));
    CHECK(readBody(&r3, c, p) == RR_OK && c == "a" && p.empty());
}

static void testBodyRejects() {
    std::string c, p;
    std::string good = encodeBodyBytes("a.b", "{}");
    { std::string x = good; x[0] = 2; FakeStream s = makeStream(true, x); CHECK(readBody(&s, c, p) == RR_BAD_PROTO); }
    { std::string x = good; x[1] = 0; x[2] = 0; FakeStream s = makeStream(true, x); CHECK(readBody(&s, c, p) == RR_BAD_CHANNEL); }
    { std::string x = good; x[1] = 65; FakeStream s = makeStream(true, x); CHECK(readBody(&s, c, p) == RR_BAD_CHANNEL); }
    { FakeStream s = makeStream(true, encodeBodyBytes("A.b", "{}")); CHECK(readBody(&s, c, p) == RR_BAD_CHANNEL); }
    { FakeStream s = makeStream(true, encodeBodyBytes("a b", "{}")); CHECK(readBody(&s, c, p) == RR_BAD_CHANNEL); }
    { std::string x = encodeBodyBytes("a.b", ""); uint32_t big = kMaxPayload + 1; memcpy(&x[x.size() - 4], &big, 4);
      FakeStream s = makeStream(true, x); CHECK(readBody(&s, c, p) == RR_TOO_BIG); }
    // Truncated bodies invalidate the stream and are reported as such, without reading past the end.
    for (size_t cut = 0; cut < good.size(); ++cut) {
        FakeStream s = makeStream(true, good.substr(0, cut));
        CHECK(readBody(&s, c, p) == RR_STREAM);
    }
    // Declared payload longer than the frame.
    { std::string x = good; uint32_t n = 1000; memcpy(&x[x.size() - 2 - 4], &n, 4); FakeStream s = makeStream(true, x); CHECK(readBody(&s, c, p) == RR_STREAM); }
}

static void testQueue() {
    queueClear();
    CHECK(queueSize() == 0 && queuePop() == NULL);
    CHECK(queuePush("a.b", "1") && queuePush("c.d", "2"));
    Msg* m = queuePop();
    CHECK(m && m->channel == "a.b" && m->payload == "1");  // FIFO
    queueRequeueFront(m);  // failed delivery keeps the message at the head
    Msg* head = queuePop();
    Msg* second = queuePop();
    CHECK(head == m && second && second->channel == "c.d" && queuePop() == NULL);
    delete head;
    delete second;
    CHECK(g_queueBytes == 0);
    queueClear();
    for (size_t i = 0; i < kMaxQueueMsgs; ++i) CHECK(queuePush("x", "y"));
    CHECK(!queuePush("x", "y"));  // 257th message is refused
    CHECK(queueSize() == kMaxQueueMsgs);
    queueClear();
    CHECK(g_queueBytes == 0);
    std::string big(kMaxPayload, 'p');
    size_t fit = 0;
    while (queuePush("c", big)) ++fit;
    CHECK(fit == kMaxQueueBytes / (kMaxPayload + 1));  // byte budget (1 MB) reached before the message budget
    CHECK(queueSize() == fit);
    queueClear();
    CHECK(queuePush("c", big));
    queueClear();
}

static void testBundle() {
    const char text[] = "--@@ext chunk core.lua\nlocal a = 1\n--@@ext chunk m.lua\r\nreturn 2\n";
    std::vector<Chunk> c = splitBundle(text, sizeof(text) - 1);
    CHECK(c.size() == 2);
    CHECK(c[0].name == "core.lua" && c[0].text == "local a = 1\n");
    CHECK(c[1].name == "m.lua" && c[1].text == "return 2\n");
    CHECK(splitBundle("", 0).empty());
    CHECK(splitBundle("no marker\n", 10).empty());
}

static void testGate() {
    resetState();
    // Wrong build: refused for good.
    BYTE* bad = makeImage(true, true);
    ((IMAGE_NT_HEADERS*)(bad + 0x80))->FileHeader.TimeDateStamp ^= 1;
    netStepWith(bad, 1000);
    CHECK(g_gate == -1 && !g_sendOk && !g_recvOk);
    VirtualFree(bad, 0, MEM_RELEASE);

    // Right header, code not yet unpacked: wait, do not claim anything.
    resetState();
    BYTE* img = makeImage(false, false);
    netStepWith(img, 1000);
    CHECK(g_gate == 1 && !g_sendOk && !g_recvOk && !g_netDone);
    // Unpacked but registry not populated yet.
    makeImage(false, false);
    { BYTE* tmp = makeImage(true, false); memcpy(img + RVA_Send, tmp + RVA_Send, 5); memcpy(img + RVA_HdrSerialize, tmp + RVA_HdrSerialize, 15);
      memcpy(img + RVA_PktStub, tmp + RVA_PktStub, 3); memcpy(img + RVA_Dispatcher, tmp + RVA_Dispatcher, 5); VirtualFree(tmp, 0, MEM_RELEASE); }
    netStepWith(img, 2000);
    CHECK(g_sendOk && !g_recvOk && !g_netDone);
    CHECK(g_pktVtbl[0] == (void*)(img + RVA_PktStub) && g_pktVtbl[1] == (void*)(img + RVA_HdrSerialize));
    CHECK(g_pktVtbl[2] == (void*)&pktBodySerializer && g_pktVtbl[5] == (void*)&pktBodySerializer);
    // Registry populated: first candidate (0x1F2) is claimed.
    { BYTE* tmp = makeImage(true, true);
      *(void**)(img + RVA_ScTable) = img + RVA_ScTableVtbl;
      for (int op = 0; op < kScSlots; ++op) *scSlot(img, (unsigned)op) = *scSlot(tmp, (unsigned)op);
      VirtualFree(tmp, 0, MEM_RELEASE); }
    netStepWith(img, 3000);
    CHECK(g_recvOk && g_netDone && g_scOpcode == 0x1F2);
    CHECK(*scSlot(img, 0x1F2) == (void*)&g_hdlObj && *scFlag(img, 0x1F2) == 1);
    CHECK(*scSlot(img, 0x1D5) == NULL && *scFlag(img, 0x1D5) == 0);  // other candidates untouched
    // Unload restores the client's table.
    restoreHandler(img);
    CHECK(*scSlot(img, 0x1F2) == NULL && *scFlag(img, 0x1F2) == 0 && !g_recvOk);
    VirtualFree(img, 0, MEM_RELEASE);

    // Candidate order: busy slots are skipped; all busy -> none.
    img = makeImage(true, true);
    *scSlot(img, 0x1F2) = (void*)1; *scSlot(img, 0x1D5) = (void*)1; *scSlot(img, 0x159) = (void*)1;
    CHECK(registryReady(img) && pickOpcode(img) == 0x14F);
    for (size_t i = 0; i < sizeof(kScCandidates) / sizeof(kScCandidates[0]); ++i) *scSlot(img, kScCandidates[i]) = (void*)1;
    CHECK(pickOpcode(img) == -1);
    // A foreign registry (wrong vtable) or sparse table is not "ready".
    *(void**)(img + RVA_ScTable) = NULL;
    CHECK(!registryReady(img));
    *(void**)(img + RVA_ScTable) = img + RVA_ScTableVtbl;
    *scSlot(img, kScKnownBusy) = NULL;
    CHECK(!registryReady(img));
    VirtualFree(img, 0, MEM_RELEASE);
    resetState();
}

static void testReceiveThroughDispatcher() {
    resetState();
    BYTE* img = makeImage(true, true);
    g_x2 = img;
    installHandler(img, 0x1F2);
    CHECK(g_recvOk && g_active);

    FakeStream s = makeStream(true, encodeBodyBytes("ext.debug.ping", "{\"n\":9}"));
    CHECK(dispatch(img, &s, 0x1F2) == 1);  // callee-clean 12 bytes, returns "handled"
    CHECK(queueSize() == 1 && g_received == 1);
    Msg* m = queuePop();
    CHECK(m && m->channel == "ext.debug.ping" && m->payload == "{\"n\":9}");
    delete m;

    // Malformed body: consumed (return 1), nothing queued, counted.
    FakeStream bad = makeStream(true, encodeBodyBytes("Bad Channel", "{}"));
    CHECK(dispatch(img, &bad, 0x1F2) == 1 && queueSize() == 0 && g_dropped == 1);
    // Truncated frame: stream invalid -> the stock error path (return 0).
    FakeStream cut = makeStream(true, encodeBodyBytes("a.b", "{\"k\":1}").substr(0, 9));
    CHECK(dispatch(img, &cut, 0x1F2) == 0 && queueSize() == 0);
    // Queue overflow drops the message but keeps the stream in sync.
    for (size_t i = 0; i < kMaxQueueMsgs; ++i) { FakeStream f = makeStream(true, encodeBodyBytes("a.b", "{}")); CHECK(dispatch(img, &f, 0x1F2) == 1); }
    { FakeStream f = makeStream(true, encodeBodyBytes("a.b", "{}")); CHECK(dispatch(img, &f, 0x1F2) == 1 && queueSize() == kMaxQueueMsgs); }
    queueClear();
    // Inactive (unloading): accepted and discarded.
    g_active = 0;
    { FakeStream f = makeStream(true, encodeBodyBytes("a.b", "{}")); CHECK(dispatch(img, &f, 0x1F2) == 1 && queueSize() == 0); }
    // The flag-0 entry (vtbl[1]) never builds a job and the destructor leaves the static object alone.
    typedef void* (__thiscall *Job_t)(void*, void*, int);
    typedef void* (__thiscall *Dtor_t)(void*, unsigned char);
    CHECK(((Job_t)g_hdlVtbl[1])(&g_hdlObj, NULL, 0x1F2) == NULL);
    CHECK(((Dtor_t)g_hdlVtbl[0])(&g_hdlObj, 1) == &g_hdlObj);
    restoreHandler(img);
    VirtualFree(img, 0, MEM_RELEASE);
    resetState();
}

// Stand-in for NetClient::Send: serializes like the client does (header, then body), thiscall, ret 4.
static std::string g_wire;
static void* g_sendThis;
static void __fastcall fakeSend(void* self, void*, ExtPacket* pkt) {
    g_sendThis = self;
    FakeStream w = makeStream(false);
    typedef void (__thiscall *Ser_t)(void* self, void* stream);
    // header serializer is the stock one in the client; here: hash, count, u16 type
    uint8_t hash = pkt->hash, count = pkt->count; uint16_t type = (uint16_t)pkt->type;
    ioBytes(&w, "hash", &hash, 1); ioBytes(&w, "count", &count, 1); ioBytes(&w, "type", &type, 2);
    ((Ser_t)pkt->vtbl[2])(pkt, &w);
    g_wire = w.bytes;
}

static void testSend() {
    resetState();
    BYTE* img = makeImage(true, true);
    g_x2 = img; g_gate = 1;
    buildPacketVtable(img);
    g_sendFn = (void*)&fakeSend; g_sendOk = 1;
    g_tid = GetCurrentThreadId();

    CHECK(strcmp(sendPacket("a.b", 3, "{}", 2), "not connected to a game server") == 0);  // NetClient pointer is NULL
    *(void**)(img + RVA_NetClientPtr) = (void*)0xABCD;
    CHECK(sendPacket("ext.hello", 9, "{\"proto\":1}", 11) == NULL);
    CHECK(g_sendThis == (void*)0xABCD);  // ECX = *(base + 0xB12294)
    // 00 00 = hash,count ; c0 00 = type 0x0C0 ; then the body
    CHECK(hex(g_wire) == "0000" "c000" + hex(encodeBodyBytes("ext.hello", "{\"proto\":1}")));
    CHECK(g_sent == 1);

    CHECK(sendPacket("Bad", 3, "{}", 2) != NULL);
    CHECK(sendPacket("", 0, "{}", 2) != NULL);
    std::string longName(65, 'a');
    CHECK(sendPacket(longName.data(), longName.size(), "", 0) != NULL);
    std::string tooBig(kMaxPayload + 1, 'x');
    CHECK(sendPacket("a.b", 3, tooBig.data(), tooBig.size()) != NULL);
    std::string maxPay(kMaxPayload, 'x');
    CHECK(sendPacket("a.b", 3, maxPay.data(), maxPay.size()) == NULL && g_wire.size() == 4 + 1 + 2 + 3 + 4 + kMaxPayload);
    CHECK(sendPacket("a.b", 3, "", 0) == NULL);  // empty payload allowed
    // Wrong thread.
    g_tid = GetCurrentThreadId() + 1;
    CHECK(strcmp(sendPacket("a.b", 3, "{}", 2), "AAEmuExt.Send must run on the main thread") == 0);
    g_tid = GetCurrentThreadId();
    // Disabled.
    g_sendOk = 0;
    CHECK(sendPacket("a.b", 3, "{}", 2) != NULL);

    // The packet object layout is what the client's callers build.
    ExtPacket p; memset(&p, 0, sizeof(p));
    VirtualFree(img, 0, MEM_RELEASE);
    resetState();
}

static void testFaultContainment() {
    resetState();
    // Reading a stream whose vtable function faults must not escape the SEH wrapper.
    struct Boom { static void __fastcall fault(FakeStream*, void*, const char*, void*, int) { RaiseException(0xE0000001, 0, 0, NULL); } };
    FakeStream s = makeStream(true);
    void* saved = g_fakeVtbl[0x44 / 4];
    g_fakeVtbl[0x44 / 4] = (void*)&Boom::fault;
    CHECK(recvSeh(&s) == 0 && g_dropped == 1);
    g_fakeVtbl[0x44 / 4] = saved;
    resetState();
}

int main() {
    Host host = { testLog, NULL, "aadriver test", NULL, 0 };
    g_host = host;
    qInit();
    puts("body encode/decode");  testBodyEncodeDecode();
    puts("body rejects");        testBodyRejects();
    puts("queue");               testQueue();
    puts("bundle");              testBundle();
    puts("x2game gate/slot");    testGate();
    puts("dispatcher receive");  testReceiveThroughDispatcher();
    puts("send");                testSend();
    puts("fault containment");   testFaultContainment();
    if (g_failures) { fprintf(stderr, "%d ext test check(s) failed\n", g_failures); return 1; }
    puts("extension wire body, queue, build gate, slot claim, dispatcher/Send conventions and fault containment passed");
    return 0;
}
