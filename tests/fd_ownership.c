#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc > 1) {
        for (int fd = 3; fd < 1024; ++fd) {
            char path[64], link[256];
            snprintf(path, sizeof(path), "/proc/self/fd/%d", fd);
            ssize_t n = readlink(path, link, sizeof(link) - 1);
            if (n >= 0) {
                link[n] = 0;
                if (strstr(link, "memfd:fixture"))
                    return 38;
            }
        }
    }
    write(1, "memfd fixture\n", 14);
    return 37;
}
