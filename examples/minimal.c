/*
 * examples/minimal.c — x1core C FFI 最小集成示例
 *
 * 演示宿主集成的完整生命周期：创建引擎 → 查询（候选 JSON）→ 上屏学习
 * → 再次查询观察学习生效 → 释放。覆盖内存契约的全部要点：
 *   - process 返回的字符串用 x1_string_free 释放
 *   - 引擎指针用 x1_engine_free 释放（且只释放一次）
 *   - x1_engine_version 返回静态串，不得释放
 *   - 同一引擎实例的全部调用在同一线程（Engine 非线程安全）
 *
 * 构建（Linux / macOS）：
 *   cargo build -p x1core --release
 *   cc -I x1core/include examples/minimal.c -o target/minimal \
 *      -L x1core/target/release -lx1core -lpthread -ldl -lm
 *
 * Windows（MSVC）：
 *   cargo build -p x1core --release
 *   cl /I x1core\include examples\minimal.c /Fe:target\minimal.exe ^
 *      /link x1core\target\release\x1core.lib
 *
 * 无完整种子时引擎自动回退内置单字表，本示例可直接运行；
 * 放置完整种子（见 README「词库说明」）后候选质量显著提升。
 */

#include <stdio.h>
#include "x1core_ffi.h"

int main(void) {
    printf("x1core version: %s\n", x1_engine_version());

    X1Engine *engine = x1_engine_new();
    if (engine == NULL) {
        fprintf(stderr, "engine create failed\n");
        return 1;
    }

    char *json = x1_engine_process(engine, "nihao");
    printf("process(\"nihao\") =\n  %s\n", json ? json : "(null)");
    x1_string_free(json);

    /* 用户上屏「你好」—— 学习（词频/bigram/输入映射）立即生效 */
    x1_engine_commit(engine, "你好");
    printf("commit(\"你好\") done\n");

    json = x1_engine_process(engine, "nihao");
    printf("process(\"nihao\") after commit =\n  %s\n", json ? json : "(null)");
    x1_string_free(json);

    x1_engine_free(engine);
    printf("engine freed, done\n");
    return 0;
}
