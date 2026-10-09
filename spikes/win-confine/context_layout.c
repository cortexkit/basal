#include <windows.h>
#include <stddef.h>
#include <stdio.h>

int main(void) {
    unsigned checks = 0;
#define CHECK(expression) do { \
    ++checks; \
    if (!(expression)) { printf("FAIL: %s\n", #expression); return 1; } \
} while (0)
    CHECK(sizeof(CONTEXT) == 1232);
    CHECK(__alignof(CONTEXT) == 16);
    CHECK(offsetof(CONTEXT, ContextFlags) == 48);
    CHECK(offsetof(CONTEXT, EFlags) == 68);
    CHECK(offsetof(CONTEXT, Dr0) == 72);
    CHECK(offsetof(CONTEXT, Dr1) == 80);
    CHECK(offsetof(CONTEXT, Dr2) == 88);
    CHECK(offsetof(CONTEXT, Dr6) == 104);
    CHECK(offsetof(CONTEXT, Dr7) == 112);
    CHECK(offsetof(CONTEXT, Rax) == 120);
    CHECK(offsetof(CONTEXT, Rcx) == 128);
    CHECK(offsetof(CONTEXT, Rdx) == 136);
    CHECK(offsetof(CONTEXT, Rsp) == 152);
    CHECK((CONTEXT_CONTROL | CONTEXT_INTEGER | CONTEXT_DEBUG_REGISTERS) == 0x00100013);
    printf("Windows SDK x64 CONTEXT: %u checks passed\n", checks);
    return 0;
}
