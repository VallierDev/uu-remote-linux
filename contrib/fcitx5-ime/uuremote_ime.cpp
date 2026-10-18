// uuremote_ime.cpp — fcitx5 addon that commits text handed to it over a local socket.
//
// UU 远程的手机输入法把提交的文本发到被控端，本机没有协议层面的注入手段（KWin 不实现
// zwp_virtual_keyboard_manager_v1），而经过剪贴板粘贴会污染用户剪贴板。这个插件让文本直接
// 进入当前焦点输入上下文：socket 在 $XDG_RUNTIME_DIR（仅同一用户可连），文本交给 fcitx5 的
// 事件分发器后由输入上下文提交，等于用户自己用输入法打出来。
#include <atomic>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>
#include <thread>

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputcontextmanager.h>
#include <fcitx/instance.h>
#include <fcitx-utils/log.h>

namespace {

constexpr const char *kSocketName = "uuremote-ime.sock";
constexpr size_t kMaxText = 64 * 1024;

class UuRemoteIme final : public fcitx::AddonInstance {
public:
    explicit UuRemoteIme(fcitx::Instance *instance)
        : instance_(instance), alive_(std::make_shared<std::atomic<bool>>(true)) {
        start();
    }

    ~UuRemoteIme() override {
        alive_->store(false);
        // Closing the listening socket makes the blocked accept() return, so the thread can end.
        if (listen_fd_ >= 0) { ::close(listen_fd_); listen_fd_ = -1; }
        if (thread_.joinable()) { thread_.join(); }
        if (!path_.empty()) { ::unlink(path_.c_str()); }
    }

private:
    void start() {
        const char *dir = ::getenv("XDG_RUNTIME_DIR");
        if (!dir || !*dir) { return; }
        path_ = std::string(dir) + "/" + kSocketName;
        struct sockaddr_un address;
        std::memset(&address, 0, sizeof(address));
        address.sun_family = AF_UNIX;
        if (path_.size() >= sizeof(address.sun_path)) { return; }
        std::memcpy(address.sun_path, path_.c_str(), path_.size());
        int fd = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
        if (fd < 0) { return; }
        ::unlink(path_.c_str()); // a crashed instance may have left the path behind
        if (::bind(fd, reinterpret_cast<struct sockaddr *>(&address), sizeof(address)) != 0) { ::close(fd); return; }
        ::chmod(path_.c_str(), S_IRUSR | S_IWUSR);
        if (::listen(fd, 4) != 0) { ::close(fd); ::unlink(path_.c_str()); return; }
        listen_fd_ = fd;
        auto alive = alive_;
        thread_ = std::thread([this, alive] {
            while (alive->load()) {
                int client = ::accept(listen_fd_, nullptr, nullptr);
                if (client < 0) { break; }
                std::string text;
                char buffer[8192];
                ssize_t count;
                while (text.size() < kMaxText && (count = ::read(client, buffer, sizeof(buffer))) > 0) {
                    text.append(buffer, static_cast<size_t>(count));
                }
                ::close(client);
                if (!text.empty()) { commit(text); }
            }
        });
    }

    /// fcitx5 is single threaded: hand the text to its dispatcher before touching an input context.
    void commit(const std::string &text) {
        auto alive = alive_;
        std::string payload = text;
        instance_->eventDispatcher().schedule([this, alive, payload = std::move(payload)] {
            if (!alive->load()) { return; }
            auto *context = instance_->inputContextManager().mostRecentInputContext();
            // 只记录字节数：输入内容不写日志。
            FCITX_INFO() << "uuremote_ime: commit " << payload.size() << " bytes, focused=" << (context != nullptr);
            if (context) { context->commitString(payload); }
        });
    }

    fcitx::Instance *instance_;
    std::shared_ptr<std::atomic<bool>> alive_;
    std::string path_;
    int listen_fd_ = -1;
    std::thread thread_;
};

class UuRemoteImeFactory final : public fcitx::AddonFactory {
public:
    fcitx::AddonInstance *create(fcitx::AddonManager *manager) override {
        return new UuRemoteIme(manager->instance());
    }
};

} // namespace

FCITX_ADDON_FACTORY(UuRemoteImeFactory);