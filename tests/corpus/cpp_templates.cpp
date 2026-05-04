/*
 * C++ template and class exercises.
 * Tests: templates, namespaces, access specifiers, constructors, RAII.
 */

#include <cstddef>
#include <stdexcept>
#include <utility>

namespace funky_test {

template <typename T, size_t N>
class FixedStack {
public:
    FixedStack() : top_(0) {}

    void push(const T &v) {
        if (top_ >= N) {
            throw std::overflow_error("FixedStack: overflow");
        }
        data_[top_++] = v;
    }

    T pop() {
        if (top_ == 0) {
            throw std::underflow_error("FixedStack: underflow");
        }
        return data_[--top_];
    }

    const T &peek() const {
        if (top_ == 0) {
            throw std::underflow_error("FixedStack: empty");
        }
        return data_[top_ - 1];
    }

    bool  empty() const { return top_ == 0; }
    size_t size()  const { return top_; }

private:
    T      data_[N];
    size_t top_;
};

/* RAII guard — calls a callable on scope exit. */
template <typename F>
class ScopeExit {
public:
    explicit ScopeExit(F &&f) : fn_(std::forward<F>(f)), active_(true) {}
    ~ScopeExit() {
        if (active_) fn_();
    }

    ScopeExit(const ScopeExit &)            = delete;
    ScopeExit &operator=(const ScopeExit &) = delete;

    void release() { active_ = false; }

private:
    F    fn_;
    bool active_;
};

template <typename F>
ScopeExit<F> make_scope_exit(F &&f) {
    return ScopeExit<F>(std::forward<F>(f));
}

/* Pair of two values (minimal std::pair replacement for illustration). */
template <typename A, typename B>
struct Pair {
    A first;
    B second;

    Pair(A a, B b) : first(std::move(a)), second(std::move(b)) {}
};

template <typename A, typename B>
Pair<A, B> make_pair(A a, B b) {
    return Pair<A, B>(std::move(a), std::move(b));
}

} // namespace funky_test

int main() {
    using namespace funky_test;

    FixedStack<int, 8> stk;
    for (int i = 0; i < 5; i++) {
        stk.push(i * 10);
    }

    int cleaned = 0;
    auto guard = make_scope_exit([&]() { cleaned = 1; });

    while (!stk.empty()) {
        (void)stk.pop();
    }

    guard.release();

    auto p = make_pair(42, 3.14);
    (void)p;

    return cleaned == 0 ? 0 : 1;
}
