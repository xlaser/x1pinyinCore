//! 双拼发布验收测试。
//!
//! 分两层：
//! 1. **decode 键位矩阵**：每套方案的典型词句 + 边界（yu/零声母/悬尾/分隔符/
//!    ü/多值消歧/er/ing/非法输入）逐键验证 decode_buffer 正确性。
//! 2. **engine 真实词句**：用户视角输入 → process 候选出词验证
//!    （词库命中 + 排序合理 + 与全拼一致）。
//!
//! 词句键位由 shuangpin.rs final_map/initial_remap 数据推导（与各方案公开
//! 键位表逐键核对过）。写错键位 = 本测试失败，即发现数据或推导错误。

use super::*;
use crate::shuangpin::{decode_buffer, ShuangpinScheme};

/// decode 层辅助：断言 (输入, 期望音节序列)。
fn assert_decode(scheme: &ShuangpinScheme, input: &str, expected: &[&str]) {
    let got = decode_buffer(scheme, input);
    let want: Option<Vec<String>> = Some(expected.iter().map(|s| s.to_string()).collect());
    assert_eq!(
        got, want,
        "方案 {} 输入 {} 解码错误",
        scheme.display_name, input
    );
}

/// decode 层辅助：断言解码失败（返回 None）。
fn assert_decode_none(scheme: &ShuangpinScheme, input: &str) {
    assert!(
        decode_buffer(scheme, input).is_none(),
        "方案 {} 输入 {} 应解码失败（None）",
        scheme.display_name,
        input
    );
}

fn scheme(id: &str) -> ShuangpinScheme {
    ShuangpinScheme::builtin_by_id(id).unwrap().clone()
}

// ═══════════════════════ 1. 典型词句 decode 矩阵 ═══════════════════════

#[test]
fn accept_xiaohe_common_words() {
    let s = scheme("xiaohe");
    // 中国 zhongguo = v(s=iong/ong) + g(uo/o)
    assert_decode(&s, "vsgo", &["zhong", "guo"]);
    // 你好 nihao = n(i) + h(ao)
    assert_decode(&s, "nihc", &["ni", "hao"]);
    // 我们 women = w(uo) + m(en)
    assert_decode(&s, "womf", &["wo", "men"]);
    // 学习 xuexi = x(ue) + x(i)
    assert_decode(&s, "xtxi", &["xue", "xi"]);
    // 吃饭 chifan = i(ch重映射, i) + f(an)
    assert_decode(&s, "iifj", &["chi", "fan"]);
    // 阿姨 ayi = aa(零声母双写) + y(i)
    assert_decode(&s, "aayi", &["a", "yi"]);
    // 再见 zaijian = z(ai=d) + j(ian=m)
    assert_decode(&s, "zdjm", &["zai", "jian"]);
    // 拼音加加？不——小鹤：拼音 pinyin = p(in) + y(ing? y=un! ing=k) → pinyin = p+k+y? 三音节
    // pinyin = pin + yin = p(in) + k(ing)? yin = y+in? y 键=un…… yin = y + in？in 在哪？
    // 小鹤 in 无单独键？b=in！→ yin = y+b = "yb"。pinyin = p+b+y+b = "pbyb"
    assert_decode(&s, "pbyb", &["pin", "yin"]);
    // 工作 gongzuo = g(eng? 不——gong = g+ong! s=iong/ong → gong = g+s = "gs") + z(uo? z=ou!)
    // zuo = z+uo = "zo"。gongzuo = "gszo"
    assert_decode(&s, "gszo", &["gong", "zuo"]);
}

#[test]
fn accept_ziranma_common_words() {
    let s = scheme("ziranma");
    // 中国（s=iong/ong, o=uo）
    assert_decode(&s, "vsgo", &["zhong", "guo"]);
    // 你好（h=ang? 自然码 h=ang；hao = h+ao? ao=k → "hk"）
    assert_decode(&s, "nihk", &["ni", "hao"]);
    // 我们（m=ian? men = m+en = m+f? 自然码 f=en → "mf"）
    assert_decode(&s, "womf", &["wo", "men"]);
    // 再见（zai = z+ai(l), jian = j+ian(m)）
    assert_decode(&s, "zljm", &["zai", "jian"]);
    // 学习（x=ie! xue = x+ue? t=[ue,ve] → "xt"）
    assert_decode(&s, "xtxi", &["xue", "xi"]);
    // 电脑 diannao = d(ian? 自然码 d=[iang,uang]! ian=m) → dian = d+ian = "dm"
    // nao = n+ao = "nk"。diannao = "dmnk"
    assert_decode(&s, "dmnk", &["dian", "nao"]);
}

#[test]
fn accept_microsoft_common_words() {
    let s = scheme("microsoft");
    // 中国
    assert_decode(&s, "vsgo", &["zhong", "guo"]);
    // 你好（hao = h+ao, ao=k → "hk"）
    assert_decode(&s, "nihk", &["ni", "hao"]);
    // 我们（m=ian! men = m+en = "mf"）
    assert_decode(&s, "womf", &["wo", "men"]);
    // 再见（zai = z+ai(l), jian = j+ian(m)）
    assert_decode(&s, "zljm", &["zai", "jian"]);
    // 学习（xue = x+ue, t=[ue,ve] → "xt"）
    assert_decode(&s, "xtxi", &["xue", "xi"]);
    // 电脑（dian = d+ian = "dm", nao = n+ao = "nk"）
    assert_decode(&s, "dmnk", &["dian", "nao"]);
    // 工作（gong = g+ong = "gs", zuo = z+uo = "zo"）
    assert_decode(&s, "gszo", &["gong", "zuo"]);
    // 明天 mingtian = m(ing? m=ian! ing=; → ming = m+; = "m;") + t(ian? t=[ue,ve]! ian=m → tian = t+ian = "tm"）
    // mingtian = "m;tm"
    assert_decode(&s, "m;tm", &["ming", "tian"]);
}

#[test]
fn accept_sogou_same_as_microsoft_words() {
    let sg = scheme("sogou");
    let ms = scheme("microsoft");
    // 搜狗 = 微软键位：典型词句逐条对照
    for input in &["vsgo", "nihk", "womf", "zejj", "gszo", "m;tm", "yy"] {
        assert_eq!(
            decode_buffer(&sg, input),
            decode_buffer(&ms, input),
            "搜狗与微软对 {} 解码应一致",
            input
        );
    }
}

#[test]
fn accept_abc_common_words() {
    let s = scheme("abc");
    // 中国（a=zh 声母! s=iong/ong → zhong = a+s）
    assert_decode(&s, "asgo", &["zhong", "guo"]);
    // 你好（hao = h+ao, ao=k → "hk"）
    assert_decode(&s, "nihk", &["ni", "hao"]);
    // 我们（m=ian! men = m+en = "mf"）
    assert_decode(&s, "womf", &["wo", "men"]);
    // 中文（zhong = a+s, wen = w+en? w=ian! en=f → wen = w+f）
    assert_decode(&s, "aswf", &["zhong", "wen"]);
    // 吃饭（chi = e 声母+韵母 i → "ei"；fan = f+an = "fj"）
    assert_decode(&s, "eifj", &["chi", "fan"]);
    // 再见（zai = z+ei = "zq"；jian = j+an = "jj"）
    assert_decode(&s, "zljw", &["zai", "jian"]);
}

#[test]
fn accept_pyjj_common_words() {
    let s = scheme("pyjj");
    // 中国（v=zh 声母, s=iong/ong）
    assert_decode(&s, "vygo", &["zhong", "guo"]);
    // 你好（hao = h+ao, ao=d → "hd"）
    assert_decode(&s, "nihd", &["ni", "hao"]);
    // 我们（men = m+en? m=ie! en=r → "mr"）
    assert_decode(&s, "womr", &["wo", "men"]);
    // 中文（wen = w+en? w=ei! en=r → "wr"）
    assert_decode(&s, "vywr", &["zhong", "wen"]);
    // 学习（xue = x+ue = "xx"；xi = x+i = "xi"）
    assert_decode(&s, "xxxi", &["xue", "xi"]);
    // 再见（zai = z+ei = "zw"；jian = j+ian = "jj"）
    assert_decode(&s, "zsjj", &["zai", "jian"]);
    // 明天（ming = m+ing = "mq"；tian = t+ian = "tj"）
    assert_decode(&s, "mqtj", &["ming", "tian"]);
}

#[test]
fn accept_ziguang_common_words() {
    let s = scheme("ziguang");
    // 中国（a=zh 声母, s=iong/ong）
    assert_decode(&s, "aygo", &["zhong", "guo"]);
    // 你好（hao = h+ao? h=[iang,uang]! ao=d → "hd"）
    assert_decode(&s, "nihd", &["ni", "hao"]);
    // 我们（men = m+en? m=ie! en=r → "mr"）
    assert_decode(&s, "womr", &["wo", "men"]);
    // 中文（zhong = a+s; wen = w+en = "wr"）
    assert_decode(&s, "aywr", &["zhong", "wen"]);
    // 学习（xue = x+ue? x=[v,uai]! ue=v → "xv"；xi = x+i）
    assert_decode(&s, "xvxi", &["xue", "xi"]);
    // 再见（zai = z+ei = "zw"；jian = j+ian = "jj"）
    assert_decode(&s, "zsjj", &["zai", "jian"]);
}

#[test]
fn accept_shoudao_common_words() {
    let s = scheme("shoudao");
    // 中国（zh=V, ong=H, g, uo=O）
    assert_decode(&s, "vhgo", &["zhong", "guo"]);
    // 你好（ni + hao=H? ao 在 D 键 → "hd"）
    assert_decode(&s, "nihd", &["ni", "hao"]);
    // 我们（wo=w+o；men = m+en? en 在 K 键 → "mk"）
    assert_decode(&s, "womk", &["wo", "men"]);
    // 中文（zhong=vh; wen = w+en，en 在 K 键 → "wk"）
    assert_decode(&s, "vhwk", &["zhong", "wen"]);
    // 学习（xue = x+ue(L 键)；xi = x+i）
    assert_decode(&s, "xlxi", &["xue", "xi"]);
    // 首道（官网自证：ES+DD）
    assert_decode(&s, "esdd", &["shou", "dao"]);
    // 女略（ü 双轨：nve/lve 走 B 键）
    assert_decode(&s, "nblb", &["nve", "lve"]);
}

// ═══════════════════════ 2. 跨方案边界矩阵 ═══════════════════════

#[test]
fn accept_yu_syllable_all_schemes() {
    // yu（雨/语/鱼/与/玉/于）——发布阻断项的键位守护
    let cases: [(&str, &str); 8] = [
        ("xiaohe", "yv"),
        ("ziranma", "yv"),
        ("microsoft", "yy"),
        ("sogou", "yy"),
        ("abc", "yv"),
        ("pyjj", "yv"),
        ("ziguang", "yx"),
        ("shoudao", "yv"),
    ];
    for (id, code) in cases {
        let s = scheme(id);
        let got = decode_buffer(&s, code);
        assert_eq!(
            got,
            Some(vec!["yv".to_string()]),
            "方案 {} 的 {} 应解码为 yv（ü 代码写法）",
            id,
            code
        );
        // engine 归一化 yv→yu 后应能查词（见 accept_engine_yu_queries）
    }
}

#[test]
fn accept_zero_initial_all_schemes() {
    // 零声母：双写/字面/o 前缀/er/ing 各方案特例
    let xiaohe = scheme("xiaohe");
    assert_decode(&xiaohe, "aa", &["a"]);
    assert_decode(&xiaohe, "oo", &["o"]);
    assert_decode(&xiaohe, "ee", &["e"]);
    assert_decode(&xiaohe, "er", &["er"]); // 字面 er
    assert_decode(&xiaohe, "ai", &["ai"]); // 字面韵母

    let ms = scheme("microsoft");
    assert_decode(&ms, "oa", &["a"]); // o 前缀
    assert_decode(&ms, "oo", &["o"]);
    assert_decode(&ms, "or", &["er"]); // r→er 特例
    assert_decode(&ms, "o;", &["ing"]); // ;→ing 特例
    assert_decode(&ms, "ou", &["ou"]); // 字面 ou

    // 首道（Custom 记忆码表）：a 家族 A 前缀、e 家族 U 前缀、o 双写
    let sd = scheme("shoudao");
    assert_decode(&sd, "aa", &["a"]);
    assert_decode(&sd, "ai", &["ai"]);
    assert_decode(&sd, "an", &["an"]);
    assert_decode(&sd, "ay", &["ang"]);
    assert_decode(&sd, "ao", &["ao"]);
    assert_decode(&sd, "ue", &["e"]);
    assert_decode(&sd, "ui", &["ei"]);
    assert_decode(&sd, "en", &["en"]);
    assert_decode(&sd, "uf", &["eng"]);
    assert_decode(&sd, "er", &["er"]);
    assert_decode(&sd, "oo", &["o"]);
    assert_decode(&sd, "ou", &["ou"]);
    // 正交：与 sh 声母路径共存（"ei"=shi 而非零声母诶——诶打 "ui"）
    assert_decode(&sd, "ei", &["shi"]);

    let abc = scheme("abc");
    assert_decode(&abc, "oa", &["a"]);
    assert_decode(&abc, "or", &["er"]); // r 键=[er,iu] 映射命中
    assert_decode(&abc, "yy", &["ying"]); // y=ing，零声母 ying 双写
    assert_decode_none(&abc, "oy"); // 零声母 ing 无编码（官方无 oy）

    let pyjj = scheme("pyjj");
    assert_decode(&pyjj, "aa", &["a"]);
    assert_decode(&pyjj, "eq", &["er"]); // er=eq（q 键=er/ing）
    assert_decode(&pyjj, "oq", &["er"]); // o 前缀双编码
    assert_decode(&pyjj, "yq", &["ying"]); // ying=y+ing

    let ziguang = scheme("ziguang");
    assert_decode(&ziguang, "oa", &["a"]);
    assert_decode(&ziguang, "oq", &["er"]); // er=oq（q=er）
    assert_decode(&ziguang, "or", &["en"]); // r=en 映射优先，不被 er 特例误伤
    assert_decode(&ziguang, "o;", &["ing"]); // ;→ing
}

#[test]
fn accept_trailing_partial_and_separator() {
    // 悬尾声母：完整音节 + 末尾单键声母
    let xiaohe = scheme("xiaohe");
    assert_decode(&xiaohe, "vsg", &["zhong", "g"]);
    assert_decode(&xiaohe, "v", &["zh"]); // 单键 = 声母候选
    assert_decode(&xiaohe, "uu", &["shu"]); // sh 重映射 + 韵母 u
    assert_decode(&xiaohe, "ii", &["chi"]); // ch 重映射 + 韵母 i

    // 分隔符
    assert_decode(&xiaohe, "vs'go", &["zhong", "guo"]);
    // 微软零声母 an = o 前缀 + j 键(an) = "oj"；du'an = du + an
    assert_decode(&ms_if_any(), "du'oj", &["du", "an"]);

    // 悬尾合法性：o/a/e 是有效音节按完整处理（设计使然）
    assert_decode(&xiaohe, "vso", &["zhong", "o"]);
}

fn ms_if_any() -> ShuangpinScheme {
    scheme("microsoft")
}

#[test]
fn accept_multi_value_disambiguation() {
    // 多值键消歧（首个合法组合胜出，无错误消歧——审计逐键核查过）
    let xiaohe = scheme("xiaohe");
    assert_decode(&xiaohe, "xk", &["xing"]); // k=[ing,uai]：x+ing 优先
    assert_decode(&xiaohe, "gk", &["guai"]); // g+uai（g+ing 无效）
    assert_decode(&xiaohe, "js", &["jiong"]); // s=[iong,ong]：j+iong（j+ong 无效）
    assert_decode(&xiaohe, "gs", &["gong"]); // g+ong（g+iong 无效）
    assert_decode(&xiaohe, "lx", &["lia"]); // l+ia（l+ua 无效）
    assert_decode(&xiaohe, "gx", &["gua"]); // g+ua
    assert_decode(&xiaohe, "hv", &["hui"]); // v=[ui,v]：h+ui
    assert_decode(&xiaohe, "jv", &["jv"]); // j+ü=v（decode 层不归一化，engine 层 jv→ju）

    let ziguang = scheme("ziguang");
    assert_decode(&ziguang, "jv", &["jue"]); // v=[ue,ve,ui]：j+ue
    assert_decode(&ziguang, "hv", &["hui"]); // h+ui
    assert_decode(&ziguang, "nx", &["nv"]); // x=[v,uai]：n+ü=nü
}

#[test]
fn accept_invalid_inputs_rejected() {
    let xiaohe = scheme("xiaohe");
    assert_decode_none(&xiaohe, "");
    assert_decode_none(&xiaohe, "vs1go"); // 非法字符
    assert_decode_none(&xiaohe, "vsgo!"); // 非法字符
    assert_decode_none(&xiaohe, "zq"); // z+iu 无效组合
}

// ═══════════════════════ 3. engine 层真实词句验收 ═══════════════════════

/// engine 辅助：双拼输入 → 候选文本列表。
fn process_sp(engine: &mut Engine, scheme_id: &str, input: &str) -> Vec<String> {
    let s = ShuangpinScheme::builtin_by_id(scheme_id).unwrap().clone();
    engine.set_shuangpin_scheme(Some(s));
    engine
        .process(input)
        .iter()
        .map(|c| c.text.as_ref().to_owned())
        .collect()
}

// 双拼全拼还原串（「双拼时显示全拼音节」的引擎侧实现）：
// 双拼模式 → ' 分隔全拼；非双拼 / 临时全拼（ffp）/ 解码失败 → None。
#[test]
fn sp_full_display_restores_syllables() {
    let mut engine = Engine::new();

    // 非双拼：恒 None
    assert_eq!(engine.shuangpin_full_display("vsgo"), None);

    // 双拼模式：2 键音节还原为 ' 分隔全拼
    let xiaohe = ShuangpinScheme::builtin_by_id("xiaohe").unwrap().clone();
    engine.set_shuangpin_scheme(Some(xiaohe));
    assert_eq!(
        engine.shuangpin_full_display("vsgo").as_deref(),
        Some("zhong'guo")
    );

    // 临时全拼（/q）：buffer 已是全拼，不再还原
    engine.force_fullpinyin = true;
    assert_eq!(engine.shuangpin_full_display("zhongguo"), None);
    engine.force_fullpinyin = false;

    // 解码失败（非法双拼组合）→ None（DLL 回退原显示）
    assert_eq!(engine.shuangpin_full_display("zq"), None);

    engine.set_shuangpin_scheme(None);
}

#[test]
fn accept_engine_common_word_queries() {
    let mut engine = Engine::new();
    // 每方案代表词：用户视角「打这个词 → 出这个词」
    let cases: [(&str, &str, &str); 8] = [
        ("xiaohe", "vsgo", "中国"),
        ("ziranma", "vsgo", "中国"),
        ("microsoft", "vsgo", "中国"),
        ("sogou", "vsgo", "中国"),
        ("abc", "asgo", "中国"),
        ("pyjj", "vygo", "中国"),
        ("ziguang", "aygo", "中国"),
        ("shoudao", "vhgo", "中国"),
    ];
    for (id, code, word) in cases {
        let texts = process_sp(&mut engine, id, code);
        assert!(
            texts.iter().any(|t| t == word),
            "方案 {} 输入 {} 应出「{}」: {:?}",
            id,
            code,
            word,
            texts
        );
        engine.set_shuangpin_scheme(None); // 清掉方案避免影响下一轮
    }
}

#[test]
fn accept_engine_yu_queries() {
    // yu 音节真实出词（发布阻断修复的 engine 层验证）：雨/语/鱼/与 是词库高频字
    let mut engine = Engine::new();
    let cases: [(&str, &str); 3] = [
        ("xiaohe", "yv"),    // 小鹤 v 键 ü
        ("microsoft", "yy"), // 微软 y 键 ü
        ("ziguang", "yx"),   // 紫光 x 键 ü
    ];
    for (id, code) in cases {
        let texts = process_sp(&mut engine, id, code);
        assert!(
            texts.iter().any(|t| t.contains("雨")
                || t.contains("鱼")
                || t.contains("语")
                || t.contains("与")),
            "方案 {} 输入 {} 应出 yu 音节字（雨/鱼/语/与）: {:?}",
            id,
            code,
            texts
        );
        engine.set_shuangpin_scheme(None);
    }
}

#[test]
fn accept_engine_zero_initial_queries() {
    // 零声母真实出词：小鹤 aa→啊/阿、微软 oa→啊、紫光 oq→儿、微软 o;→应/英
    let mut engine = Engine::new();
    let cases: [(&str, &str, &str); 7] = [
        ("xiaohe", "aa", "啊"),
        ("microsoft", "oa", "啊"),
        ("ziguang", "oq", "儿"),
        ("microsoft", "o;", "应"),
        // 首道（Custom 记忆码）：aa→啊、ue→俄/额、ui→诶、ay→昂
        ("shoudao", "aa", "啊"),
        ("shoudao", "ue", "俄"),
        ("shoudao", "ay", "昂"),
    ];
    for (id, code, expect) in cases {
        let texts = process_sp(&mut engine, id, code);
        assert!(
            texts.iter().any(|t| t.contains(expect) || *t == expect),
            "方案 {} 输入 {} 应出「{}」: {:?}",
            id,
            code,
            expect,
            texts
        );
        engine.set_shuangpin_scheme(None);
    }
}

#[test]
fn accept_engine_trailing_partial_query() {
    // 悬尾声母真实出词：vsg = zhong + g → 「中国」应可达（g 是 guo 的首字母简拼尾）
    let mut engine = Engine::new();
    let texts = process_sp(&mut engine, "xiaohe", "vsg");
    assert!(
        texts.iter().any(|t| t.contains("中")),
        "小鹤 vsg（zhong+g）应出 zhong 开头的候选: {:?}",
        texts
    );
    // 对照：完整 vsgo 首候选是中国
    let texts2 = process_sp(&mut engine, "xiaohe", "vsgo");
    assert_eq!(texts2.first().map(|s| s.as_str()), Some("中国"));
}

/// 悬尾声母的整句候选——与全拼悬尾路径同构。
/// faxmyigew = faxm(fa xian) + yi + ge + w（悬尾声母）→ 全消费整句「发现一个我」
/// 应出且排首位。若 last_is_partial 直接跳过整句 DP，首位会停在部分覆盖的
/// 「发现」（与全拼 faxianyigew 同族缺口）。
#[test]
fn accept_engine_trailing_partial_green_sentence() {
    use crate::storage::{Storage, X1DictWord};
    // 构造式 DB（不耦合种子数据）：发现/一个/发现一个/我 —— w 悬尾的补字来源。
    let tmp = crate::test_tmp::dir().join(format!(
        "x1_sp_green_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "发现".into(),
                pinyin: "fa xian".into(),
                freq: 6306,
                ..Default::default()
            },
            X1DictWord {
                simplified: "一个".into(),
                pinyin: "yi ge".into(),
                freq: 959,
                ..Default::default()
            },
            // valid 前缀本身成词场景（全拼路径须避免在此触发整词短路）
            X1DictWord {
                simplified: "发现一个".into(),
                pinyin: "fa xian yi ge".into(),
                freq: 959,
                ..Default::default()
            },
            X1DictWord {
                simplified: "我".into(),
                pinyin: "wo".into(),
                freq: 9999,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    // 双拼悬尾：faxmyigew（xiaohe）= fa xian yi ge + w
    let sp = process_sp(&mut engine, "xiaohe", "faxmyigew");
    assert_eq!(
        sp.first().map(|s| s.as_str()),
        Some("发现一个我"),
        "悬尾整句应排首位（而非部分覆盖的「发现」）: {:?}",
        sp
    );

    // 与全拼同构对照：faxianyigew 亦应首位「发现一个我」（语义一致）
    engine.set_shuangpin_scheme(None);
    let full = engine.process("faxianyigew");
    assert_eq!(
        full.first().map(|c| c.text.as_ref()),
        Some("发现一个我"),
        "全拼对照应同样首位: {:?}",
        full.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(&tmp);
}

/// 扩展：zh/ch/sh 双字声母悬尾——整句路径与单字母悬尾同族，
/// 且补字必须按**完整声母**过滤（zh 尾补 zh 系字，不得补首字母同为 z 的 z 系字）。
/// 构造库放入频率更高的 z 系干扰字「在」（zai），无过滤时补字会错选它
/// （无过滤时全拼 faxianyigezh 会补出「发现一个在[句]」）。
#[test]
fn accept_engine_trailing_zh_shengmu_green_sentence() {
    use crate::storage::{Storage, X1DictWord};
    let tmp = crate::test_tmp::dir().join(format!(
        "x1_sp_zh_{}_{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    {
        let mut s = Storage::open(&tmp).unwrap();
        s.import_x1dict_batch(&[
            X1DictWord {
                simplified: "发现".into(),
                pinyin: "fa xian".into(),
                freq: 6306,
                ..Default::default()
            },
            X1DictWord {
                simplified: "一个".into(),
                pinyin: "yi ge".into(),
                freq: 959,
                ..Default::default()
            },
            X1DictWord {
                simplified: "发现一个".into(),
                pinyin: "fa xian yi ge".into(),
                freq: 959,
                ..Default::default()
            },
            // zh 系候选字（补字正解）
            X1DictWord {
                simplified: "中".into(),
                pinyin: "zhong".into(),
                freq: 9000,
                ..Default::default()
            },
            // z 系干扰字：频率更高——补字若只取首字母会选中它（旧 bug）
            X1DictWord {
                simplified: "在".into(),
                pinyin: "zai".into(),
                freq: 9999,
                ..Default::default()
            },
        ])
        .unwrap();
    }
    let mut engine = Engine::new_with_db(tmp.to_str().unwrap()).unwrap();

    // 双拼悬尾（xiaohe）：faxmyigev = fa xian yi ge + v(zh)
    let sp = process_sp(&mut engine, "xiaohe", "faxmyigev");
    assert_eq!(
        sp.first().map(|s| s.as_str()),
        Some("发现一个中"),
        "zh 悬尾整句应首位且补 zh 系字（不得补 z 系字「在」）: {:?}",
        sp
    );

    // 全拼同型对照（悬尾场景 + 补字过滤）：faxianyigezh
    engine.set_shuangpin_scheme(None);
    let full = engine.process("faxianyigezh");
    assert_eq!(
        full.first().map(|c| c.text.as_ref()),
        Some("发现一个中"),
        "全拼 zh 半截尾应同样补 zh 系字: {:?}",
        full.iter().map(|c| &c.text).collect::<Vec<_>>()
    );

    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn accept_engine_learn_and_memo_in_shuangpin() {
    // 双拼 commit 学习（全拼展开）+ 绿词记忆 + 跨模式：
    let mut engine = Engine::new();
    let s = ShuangpinScheme::builtin_by_id("xiaohe").unwrap().clone();
    engine.set_shuangpin_scheme(Some(s));
    // commit 自造词「中国好」（vsgohc）→ 学习 + green_memo
    engine.commit("vsgohc", "中国好", Some("vsgohc"), true);
    // 双拼查询首候选 = 中国好（green_memo lift）
    let r1 = engine.process("vsgohc");
    assert_eq!(r1.first().map(|c| c.text.as_ref()), Some("中国好"));
    // 切全拼：user_words 学习的是全拼（zhong guo hao），应可查
    engine.set_shuangpin_scheme(None);
    let r2 = engine.process("zhongguohao");
    assert!(
        r2.iter().any(|c| c.text.as_ref() == "中国好"),
        "切全拼后应查到双拼时期学习的「中国好」: {:?}",
        r2.iter().map(|c| &c.text).collect::<Vec<_>>()
    );
}
