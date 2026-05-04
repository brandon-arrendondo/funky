/*
 * 文件说明：测试 Unicode 注释和标识符处理。
 * This file tests Unicode in comments and string literals.
 * Exercice : commentaires en français et chaînes UTF-8.
 */

#include <stdio.h>
#include <string.h>

/* 最大长度 */
#define MAX_LEN 256

/* 计算字符串长度（字节数） */
static int byte_length(const char *s) {
    int n = 0;
    while (s[n]) n++;
    return n;
}

typedef struct {
    char  buf[MAX_LEN]; /* 内部缓冲区 */
    int   len;          /* 当前长度   */
} Строка; /* Russian: "string" — identifier test */

void строка_init(Строка *s) {
    memset(s->buf, 0, sizeof(s->buf));
    s->len = 0;
}

int строка_set(Строка *s, const char *src) {
    int l = byte_length(src);
    if (l >= MAX_LEN) return -1;
    memcpy(s->buf, src, l + 1);
    s->len = l;
    return 0;
}

int main(void) {
    /* 中文字符串字面量 */
    const char *msg = "你好，世界！";
    printf("%s\n", msg);

    /* Emoji in string — should pass through unchanged */
    const char *emoji = "hello 🌍";
    printf("%s\n", emoji);

    Строка s;
    строка_init(&s);
    if (строка_set(&s, "Привет") != 0) {
        fprintf(stderr, "ошибка: строка слишком длинная\n");
        return 1;
    }
    printf("строка: %s (len=%d)\n", s.buf, s.len);

    return 0;
}
