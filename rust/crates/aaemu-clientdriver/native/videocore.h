// ============================================================================
//  videocore.h - the video capture's decisions that need no GPU or ffmpeg.
//
//  * CfrScheduler: turns the irregular game frames (each with the wall time it was presented)
//    into a constant-frame-rate sequence. Output frame k of the file shows the most recent
//    game frame at t0 + k / fps; a game frame can therefore be shown several times
//    (repeated) or never (dropped).
//  * the ffmpeg command line and the `<path>.json` sidecar text.
//
//  Header only so that video-test.cpp can exercise it without a game.
// ============================================================================
#pragma once
#include <stdint.h>
#include <stdio.h>
#include <deque>
#include <string>

namespace videocore {

class CfrScheduler {
public:
    explicit CfrScheduler(int fps) : fps_(fps < 1 ? 1 : fps) {}

    // Feeds a game frame presented at wall time t_us (Unix microseconds). Frames must arrive in
    // presentation order. The first frame defines t0 (output frame 0). A frame stamped before
    // time that is already resolved is treated as presented at the resolved horizon (late).
    void add(int64_t t_us, uint64_t id) {
        if (!started_) { started_ = true; t0_ = t_us; last_t_ = t_us; }
        if (t_us < last_t_) t_us = last_t_;
        if (t_us < horizon_) { t_us = horizon_; ++late_; }
        last_t_ = t_us;
        pending_.push_back(Frame{t_us, id});
    }

    // Resolves every output frame whose time lies before horizon_us: all game frames presented
    // before the horizon must already have been added. emit(id, repeated) is called once per
    // output frame, in order.
    template <class F> void advance(int64_t horizon_us, F&& emit) {
        if (!started_) return;
        if (horizon_us > horizon_) horizon_ = horizon_us;
        for (;;) {
            int64_t tk = tick_time(next_);
            if (tk >= horizon_us) break;
            while (!pending_.empty() && pending_.front().t <= tk) {
                if (have_cur_ && !cur_shown_) ++dropped_;
                cur_ = pending_.front();
                pending_.pop_front();
                have_cur_ = true;
                cur_shown_ = false;
            }
            if (!have_cur_) break;                       // cannot happen: frame 0 is at t0
            bool repeated = cur_shown_;
            if (repeated) ++repeated_;
            cur_shown_ = true;
            ++out_;
            ++next_;
            emit(cur_.id, repeated);
        }
    }

    // Time of output frame k.
    int64_t tick_time(int64_t k) const { return t0_ + (k * 1000000LL) / fps_; }

    bool started() const { return started_; }
    int64_t t0_us() const { return t0_; }
    int fps() const { return fps_; }
    uint64_t frames_out() const { return out_; }          // frames in the file
    uint64_t repeated() const { return repeated_; }       // output frames that repeated the previous game frame
    uint64_t dropped() const { return dropped_; }         // game frames that were never shown
    uint64_t late() const { return late_; }               // frames that arrived after their time was resolved
    bool has_current() const { return have_cur_; }
    uint64_t current_id() const { return cur_.id; }       // the game frame shown by the latest output frame
    size_t pending() const { return pending_.size(); }

private:
    struct Frame { int64_t t; uint64_t id; };
    int fps_;
    bool started_ = false;
    int64_t t0_ = 0, last_t_ = 0, horizon_ = INT64_MIN;
    int64_t next_ = 0;
    std::deque<Frame> pending_;
    Frame cur_{0, 0};
    bool have_cur_ = false, cur_shown_ = false;
    uint64_t out_ = 0, repeated_ = 0, dropped_ = 0, late_ = 0;
};

// The ffmpeg command line (without the program path, which the caller quotes in front):
// raw BGRA frames on stdin, constant frame rate, H.264 for broad playback.
// The tallest picture the file gets: a taller back buffer is scaled down by ffmpeg (the side-by-side videos are 540 lines
// high). Encoding 1080p next to a game running at 140 fps kept ffmpeg to about 9 frames a second (skill parity run t1e:
// 2017 of 2903 output frames repeated); 720p is 2.25 times less work.
static const int kMaxHeight = 720;

// The size of the file's picture for a back buffer of width x height: even, at most kMaxHeight lines.
inline void out_size(int width, int height, int* out_w, int* out_h) {
    if (height > kMaxHeight) {
        *out_h = kMaxHeight;
        *out_w = (int)(((long long)width * kMaxHeight / height + 1) / 2 * 2);
    } else {
        *out_w = (width + 1) / 2 * 2;
        *out_h = (height + 1) / 2 * 2;
    }
}

// Whether a present at wall_us is copied: only the ones the output grid can show, at least 3/4 of an output frame after the
// last copy (last_us 0: none yet). A game at 140 fps then costs about 40 copies a second instead of 140.
inline bool capture_due(int64_t wall_us, int64_t last_us, int fps) {
    if (last_us == 0) return true;
    const int64_t period = 1000000 / (fps < 1 ? 1 : fps);
    return wall_us - last_us >= period * 3 / 4;
}

inline std::string ffmpeg_args(int width, int height, int fps, const std::string& out_path) {
    char head[256];
    _snprintf_s(head, sizeof(head), _TRUNCATE,
                " -y -hide_banner -loglevel error -f rawvideo -pix_fmt bgra -s %dx%d -r %d -i -", width, height, fps);
    std::string args = head;
    if (height > kMaxHeight) {
        int w = 0, h = 0;
        out_size(width, height, &w, &h);
        char scale[96];
        _snprintf_s(scale, sizeof(scale), _TRUNCATE, " -vf scale=%d:%d:flags=fast_bilinear", w, h);
        args += scale;
    } else if ((width & 1) || (height & 1)) {
        // yuv420p needs even dimensions.
        args += " -vf pad=ceil(iw/2)*2:ceil(ih/2)*2";
    }
    args += " -c:v libx264 -preset ultrafast -crf 18 -pix_fmt yuv420p \"";
    args += out_path;
    args += '"';
    return args;
}

// `<path>.json`: {t0_wall_us, fps, width, height, frames, repeated, dropped} plus diagnostics.
// `dropped` counts every game frame that is not in the file: dropped by the cadence
// (dropped_cfr, superseded before its output time), lost because the writer queue had no free
// buffer (lost_queue), or because the GPU copy of that frame was not ready (lost_gpu).
inline std::string meta_json(int64_t t0_us, int fps, int width, int height, uint64_t frames, uint64_t repeated,
                             uint64_t dropped_cfr, uint64_t lost_queue, uint64_t lost_gpu, uint64_t late,
                             long ffmpeg_exit) {
    char out[640];
    _snprintf_s(out, sizeof(out), _TRUNCATE,
                "{\"t0_wall_us\":%lld,\"fps\":%d,\"width\":%d,\"height\":%d,\"frames\":%llu,\"repeated\":%llu,"
                "\"dropped\":%llu,\"dropped_cfr\":%llu,\"lost_queue\":%llu,\"lost_gpu\":%llu,\"late\":%llu,"
                "\"ffmpeg_exit\":%ld}",
                (long long)t0_us, fps, width, height, (unsigned long long)frames, (unsigned long long)repeated,
                (unsigned long long)(dropped_cfr + lost_queue + lost_gpu), (unsigned long long)dropped_cfr,
                (unsigned long long)lost_queue, (unsigned long long)lost_gpu, (unsigned long long)late, ffmpeg_exit);
    return out;
}

} // namespace videocore
