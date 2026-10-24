//! 纯 Rust SHA-256（零第三方依赖）——评测报告的文件溯源哈希。
//!
//! 为什么内联而不调系统 `shasum`：Windows 上没有 `shasum` 可执行文件（Git Bash
//! 里那个是 perl 脚本，`std::process::Command` 无法直接 spawn），两个评测模块
//! 各自的 shell out 在 Windows 上静默失败，报告里的 db/corpus sha256 退化成
//! "unknown"——「跨轮对比先确认语料与词库版本一致」这道溯源在 Windows 开发机
//! 上从未真正生效过。FIPS 180-4 实现约百行，不破坏零依赖纪律，用已知答案
//! 测试守护正确性。
//!
//! 只服务于测试/评测链路（cfg(test)），不进发布产物。

/// SHA-256 轮常量（前 64 个素数的立方根小数部分前 32 位）。
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// 流式 SHA-256。分块喂入，避免把整个文件读进内存再拷贝一份待哈希消息。
pub(crate) struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_bytes: u64,
}

impl Sha256 {
    pub(crate) fn new() -> Self {
        Self {
            state: H0,
            buf: [0u8; 64],
            buf_len: 0,
            total_bytes: 0,
        }
    }

    pub(crate) fn update(&mut self, mut data: &[u8]) {
        self.total_bytes = self.total_bytes.wrapping_add(data.len() as u64);
        // 先补足上轮的残留块
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        // 整块直接压（不经缓冲）
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        // 尾块进缓冲
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// 收尾：补 0x80、零填充到 56 字节、追加 64 位大端比特长度。
    pub(crate) fn finish_hex(mut self) -> String {
        let bit_len = self.total_bytes.wrapping_mul(8);
        self.update(&[0x80]);
        // update 已计入长度，此处只补零到 56 mod 64
        while self.buf_len != 56 {
            self.update(&[0x00]);
        }
        // 上一步的 update 会把 total_bytes 一并算脏——长度已在调用前取好，
        // 直接手写 8 字节进缓冲并压缩，绕开 update 的计数。
        let len_bytes = bit_len.to_be_bytes();
        self.buf[56..64].copy_from_slice(&len_bytes);
        let block = self.buf;
        self.compress(&block);

        let mut out = String::with_capacity(64);
        for word in self.state {
            for b in word.to_be_bytes() {
                out.push_str(&format!("{b:02x}"));
            }
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let mut a = self.state[0];
        let mut b = self.state[1];
        let mut c = self.state[2];
        let mut d = self.state[3];
        let mut e = self.state[4];
        let mut f = self.state[5];
        let mut g = self.state[6];
        let mut h = self.state[7];
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(v);
        }
    }
}

/// 内存字节的 SHA-256 十六进制小写串。
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finish_hex()
}

/// 文件 SHA-256（64KB 分块流式）。读取失败返回 None——调用方决定降级行为。
pub(crate) fn sha256_of_file(path: &str) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
    Some(h.finish_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS 180-4 / 通用已知答案：空串与 "abc"。
    #[test]
    fn known_answer_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 448 比特消息：恰好跨两块边界（55/56/57/63/64 字节是填充逻辑的雷区）
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    /// 分块喂入与一次性喂入结果必须一致（流式实现的等价性）。
    #[test]
    fn chunked_matches_oneshot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut h = Sha256::new();
        for chunk in data.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(h.finish_hex(), sha256_hex(&data));
    }

    /// 文件哈希与同内容内存哈希一致（sha256_of_file 的读路径正确）。
    #[test]
    fn file_hash_matches_memory() {
        let p = crate::test_tmp::dir().join(format!("sha256_{}.bin", std::process::id()));
        let content = b"x1core sha256 file hash fixture\n".repeat(100);
        std::fs::write(&p, &content).unwrap();
        let got = sha256_of_file(p.to_str().unwrap()).unwrap();
        assert_eq!(got, sha256_hex(&content));
        assert_eq!(got.len(), 64);
        let _ = std::fs::remove_file(&p);
    }

    /// 不存在的文件返回 None（调用方降级），不 panic。
    #[test]
    fn missing_file_is_none() {
        assert!(sha256_of_file("Z:/nonexistent/x1-sha256-probe.bin").is_none());
    }
}
