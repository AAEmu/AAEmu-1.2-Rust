// Fixture tests for the video cadence (repeat/drop math), the ffmpeg command line and the
// sidecar JSON. No GPU, no ffmpeg, no game.
#include "videocore.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>

static int g_failures;
#define CHECK(cond) do { if (!(cond)) { fprintf(stderr, "FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond); ++g_failures; } } while (0)

struct Out { uint64_t id; bool repeated; };

static std::vector<Out> run(videocore::CfrScheduler& s, int64_t horizon) {
    std::vector<Out> out;
    s.advance(horizon, [&out](uint64_t id, bool repeated) { out.push_back(Out{id, repeated}); });
    return out;
}

// Game frames every 40 ms (25 fps) into a 10 fps file: ticks at 0, 100, 200, ... ms.
static void test_fast_game_drops() {
    videocore::CfrScheduler s(10);
    const int64_t t0 = 1700000000000000LL;
    for (uint64_t i = 0; i < 40; ++i) s.add(t0 + (int64_t)i * 40000, i);
    CHECK(s.t0_us() == t0 && s.started() && s.fps() == 10);
    // nothing is resolved before the horizon: a tick exactly at the horizon waits
    CHECK(run(s, t0).empty());
    std::vector<Out> out = run(s, t0 + 1000000);          // ticks 0..9
    CHECK(out.size() == 10);
    const uint64_t expected[10] = { 0, 2, 5, 7, 10, 12, 15, 17, 20, 22 };   // floor(2.5 k)
    for (size_t k = 0; k < out.size(); ++k) CHECK(out[k].id == expected[k] && !out[k].repeated);
    CHECK(s.frames_out() == 10 && s.repeated() == 0);
    CHECK(s.current_id() == 22 && s.has_current());
    // frames 0..22 had been presented by the last shown one; 10 of them were shown, 13 never
    CHECK(s.dropped() == 13);
}

// A slow game (5 fps) into a 10 fps file: each game frame is shown twice.
static void test_slow_game_repeats() {
    videocore::CfrScheduler s(10);
    for (uint64_t i = 0; i < 6; ++i) s.add((int64_t)i * 200000, i);
    std::vector<Out> out = run(s, 1000000);                // ticks 0..9
    CHECK(out.size() == 10);
    for (size_t k = 0; k < out.size(); ++k) {
        CHECK(out[k].id == k / 2);
        CHECK(out[k].repeated == (k % 2 == 1));
    }
    CHECK(s.repeated() == 5 && s.dropped() == 0 && s.frames_out() == 10);
}

// The game stalls (loading screen): the last frame is repeated at the file's cadence.
static void test_stall_repeats_last_frame() {
    videocore::CfrScheduler s(10);
    s.add(0, 1); s.add(40000, 2); s.add(80000, 3);
    std::vector<Out> out = run(s, 1000000);
    CHECK(out.size() == 10);
    CHECK(out[0].id == 1 && !out[0].repeated);
    CHECK(out[1].id == 3 && !out[1].repeated);              // 40 ms and 80 ms frames: only the latest at 100 ms is shown
    for (size_t k = 2; k < out.size(); ++k) CHECK(out[k].id == 3 && out[k].repeated);
    CHECK(s.dropped() == 1 && s.repeated() == 8);           // frame 2 never shown
    s.add(1500000, 4);                                      // the game resumes
    out = run(s, 2000000);                                  // ticks 10..19
    CHECK(out.size() == 10);
    for (size_t k = 0; k < 5; ++k) CHECK(out[k].id == 3 && out[k].repeated);   // ticks 10..14 (to 1.4 s)
    CHECK(out[5].id == 4 && !out[5].repeated);              // tick 15 = 1.5 s: a frame exactly at the tick time is included
    CHECK(s.late() == 0);
}

static void test_boundaries_and_clamping() {
    videocore::CfrScheduler s(10);
    s.add(1000, 1);
    CHECK(s.t0_us() == 1000 && s.tick_time(0) == 1000 && s.tick_time(1) == 101000 && s.tick_time(25) == 2501000);
    // strict horizon: a tick at the horizon is not resolved yet
    CHECK(run(s, 1000).empty());
    CHECK(run(s, 1001).size() == 1);
    CHECK(run(s, 101000).empty());                          // tick 1 is at 101000

    // timestamps that go backwards are held at the previous time: both frames are "at" 100 ms
    videocore::CfrScheduler m(10);
    m.add(100000, 1);
    m.add(50000, 2);
    std::vector<Out> out = run(m, 100001);
    CHECK(out.size() == 1 && out[0].id == 2 && !out[0].repeated);
    CHECK(m.dropped() == 1 && m.late() == 0);               // frame 1 was superseded before it was shown
    // a frame stamped before time that is already resolved is clamped to the horizon
    m.add(60000, 3);
    CHECK(m.late() == 1);
    out = run(m, 200001);                                   // tick 1 (200000)
    CHECK(out.size() == 1 && out[0].id == 3 && !out[0].repeated && m.dropped() == 1);

    // never started: advancing does nothing
    videocore::CfrScheduler idle(30);
    CHECK(!idle.started() && run(idle, 1LL << 40).empty());
    // absurd fps values are clamped to at least 1
    videocore::CfrScheduler one(0);
    CHECK(one.fps() == 1);
    // 30 fps ticks do not drift: exactly 30 per second for an hour
    videocore::CfrScheduler thirty(30);
    thirty.add(5, 0);
    CHECK(thirty.tick_time(30 * 3600) - thirty.t0_us() == 3600LL * 1000000);
    CHECK(thirty.tick_time(1) - thirty.t0_us() == 33333 && thirty.tick_time(3) - thirty.t0_us() == 100000);
}

// Random game timing, random advance steps: every output frame must show the latest game
// frame at its tick time, and the repeat/drop counters must agree with a brute-force count.
static void test_against_brute_force() {
    for (int fps : { 1, 10, 24, 30, 60, 120 }) {
        uint64_t seed = 12345 + (uint64_t)fps;
        auto next = [&seed]() { seed = seed * 6364136223846793005ULL + 1442695040888963407ULL; return (uint32_t)(seed >> 33); };
        videocore::CfrScheduler s(fps);
        std::vector<int64_t> times;
        int64_t t = 1700000000000000LL;
        std::vector<Out> all;
        int64_t horizon = 0;
        uint64_t added = 0;
        for (int round = 0; round < 400; ++round) {
            int batch = 1 + (int)(next() % 40);
            for (int i = 0; i < batch; ++i) {
                t += (int64_t)(next() % 90000);                 // 0..90 ms apart, sometimes simultaneous
                if (next() % 50 == 0) t += 700000;               // an occasional stall
                s.add(t, added);
                times.push_back(t);
                ++added;
            }
            horizon = t;                                         // everything before the newest frame is known
            std::vector<Out> out = run(s, horizon);
            all.insert(all.end(), out.begin(), out.end());
        }
        CHECK(!all.empty() && all.size() == s.frames_out());
        int64_t t0 = times[0];
        uint64_t shown_distinct = 0;
        uint64_t last_id = 0;
        bool first = true;
        for (size_t k = 0; k < all.size(); ++k) {
            int64_t tk = t0 + ((int64_t)k * 1000000LL) / fps;
            // brute force: the last frame with time <= tk
            size_t want = 0;
            for (size_t i = 0; i < times.size() && times[i] <= tk; ++i) want = i;
            CHECK(all[k].id == want);
            bool is_repeat = !first && all[k].id == last_id;
            CHECK(all[k].repeated == is_repeat);
            if (first || all[k].id != last_id) ++shown_distinct;
            CHECK(first || all[k].id >= last_id);
            last_id = all[k].id; first = false;
        }
        CHECK(s.repeated() == all.size() - shown_distinct);
        CHECK(s.dropped() == (last_id + 1) - shown_distinct);    // frames up to the last shown one that never were
    }
}

static void test_ffmpeg_args_and_meta() {
    CHECK(videocore::ffmpeg_args(1280, 720, 30, "D:\\caps\\run 1\\video.mp4") ==
          " -y -hide_banner -loglevel error -f rawvideo -pix_fmt bgra -s 1280x720 -r 30 -i -"
          " -c:v libx264 -preset ultrafast -crf 18 -pix_fmt yuv420p \"D:\\caps\\run 1\\video.mp4\"");
    CHECK(videocore::ffmpeg_args(1281, 720, 60, "o.mp4").find(" -vf pad=ceil(iw/2)*2:ceil(ih/2)*2 -c:v libx264") != std::string::npos);
    CHECK(videocore::ffmpeg_args(1280, 719, 60, "o.mp4").find("-vf pad=") != std::string::npos);
    CHECK(videocore::ffmpeg_args(1280, 720, 60, "o.mp4").find("-vf") == std::string::npos);
    // a 1080p back buffer is scaled to 720 lines (even width), with no pad
    CHECK(videocore::ffmpeg_args(1920, 1080, 30, "o.mp4").find(" -vf scale=1280:720:flags=fast_bilinear -c:v libx264") != std::string::npos);
    CHECK(videocore::ffmpeg_args(1920, 1080, 30, "o.mp4").find("pad=") == std::string::npos);
    int w = 0, h = 0;
    videocore::out_size(1920, 1080, &w, &h); CHECK(w == 1280 && h == 720);
    videocore::out_size(1366, 768, &w, &h); CHECK(w == 1280 && h == 720);
    videocore::out_size(1281, 721, &w, &h); CHECK(w == 1280 && h == 720);
    videocore::out_size(1281, 719, &w, &h); CHECK(w == 1282 && h == 720);
    videocore::out_size(1280, 720, &w, &h); CHECK(w == 1280 && h == 720);
    // a copy at least 3/4 of an output frame after the last one: 30 fps -> 33333 us * 3/4 = 24999 us
    CHECK(videocore::capture_due(1000, 0, 30));
    CHECK(!videocore::capture_due(1000 + 24998, 1000, 30));
    CHECK(videocore::capture_due(1000 + 24999, 1000, 30));
    // dropped in the sidecar covers cadence drops, queue overflow and GPU misses
    CHECK(videocore::meta_json(1700000000123456LL, 30, 1280, 720, 151, 12, 40, 3, 2, 1, 0) ==
          "{\"t0_wall_us\":1700000000123456,\"fps\":30,\"width\":1280,\"height\":720,\"frames\":151,\"repeated\":12,"
          "\"dropped\":45,\"dropped_cfr\":40,\"lost_queue\":3,\"lost_gpu\":2,\"late\":1,\"ffmpeg_exit\":0}");
    CHECK(videocore::meta_json(-1, 1, 2, 3, 0, 0, 0, 0, 0, 0, -1).find("\"ffmpeg_exit\":-1}") != std::string::npos);
}

int main() {
    test_fast_game_drops();
    test_slow_game_repeats();
    test_stall_repeats_last_frame();
    test_boundaries_and_clamping();
    test_against_brute_force();
    test_ffmpeg_args_and_meta();
    if (g_failures) { fprintf(stderr, "%d video check(s) failed\n", g_failures); return 1; }
    puts("video cadence (repeat/drop math, clamping, drift), ffmpeg command line and sidecar passed");
    return 0;
}
