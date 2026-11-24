# 第三方依赖声明

项目自身使用 [MIT 许可证](LICENSE)。本文件根据 `Cargo.lock` 对应的
Cargo 解析结果生成；包含开发依赖与其他目标平台依赖，不表示每个依赖均链接进
当前平台的二进制。源码包没有内嵌这些依赖的源代码。

表中的许可表达式来自上游 crate 元数据，`OR` 表示可选许可，`AND` 表示同时适用；
旧式斜杠表达保留上游写法。项目 MIT 许可不覆盖第三方依赖。
依赖本身的版权、许可全文与 NOTICE 以对应版本的上游源码包为准。
发布二进制时，应按实际链接的依赖保留相应版权、许可和归属声明。

重新生成：

```sh
python3 -B scripts/export_dependency_notices.py
```

该命令执行 `cargo metadata --locked --format-version 1`，只接受公开 crates.io
依赖。它可能需要下载尚未缓存的依赖元数据。

当前解析包含 **333** 个第三方 package 版本。

| 依赖 | 版本 | 上游声明许可 |
| --- | --- | --- |
| [aes](https://crates.io/crates/aes/0.8.4) | 0.8.4 | MIT OR Apache-2.0 |
| [ahash](https://crates.io/crates/ahash/0.8.12) | 0.8.12 | MIT OR Apache-2.0 |
| [aho-corasick](https://crates.io/crates/aho-corasick/1.1.5) | 1.1.5 | Unlicense OR MIT |
| [allocator-api2](https://crates.io/crates/allocator-api2/0.2.21) | 0.2.21 | MIT OR Apache-2.0 |
| [anyhow](https://crates.io/crates/anyhow/1.0.104) | 1.0.104 | MIT OR Apache-2.0 |
| [async-trait](https://crates.io/crates/async-trait/0.1.92) | 0.1.92 | MIT OR Apache-2.0 |
| [atoi](https://crates.io/crates/atoi/2.0.0) | 2.0.0 | MIT |
| [atomic-waker](https://crates.io/crates/atomic-waker/1.1.2) | 1.1.2 | Apache-2.0 OR MIT |
| [autocfg](https://crates.io/crates/autocfg/1.5.1) | 1.5.1 | Apache-2.0 OR MIT |
| [aws-lc-rs](https://crates.io/crates/aws-lc-rs/1.19.0) | 1.19.0 | ISC AND (Apache-2.0 OR ISC) |
| [aws-lc-sys](https://crates.io/crates/aws-lc-sys/0.47.0) | 0.47.0 | ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0) |
| [axum](https://crates.io/crates/axum/0.8.9) | 0.8.9 | MIT |
| [axum-core](https://crates.io/crates/axum-core/0.5.6) | 0.5.6 | MIT |
| [base64](https://crates.io/crates/base64/0.22.1) | 0.22.1 | MIT OR Apache-2.0 |
| [base64](https://crates.io/crates/base64/0.23.1) | 0.23.1 | MIT OR Apache-2.0 |
| [base64ct](https://crates.io/crates/base64ct/1.8.3) | 1.8.3 | Apache-2.0 OR MIT |
| [bit-set](https://crates.io/crates/bit-set/0.8.0) | 0.8.0 | Apache-2.0 OR MIT |
| [bit-vec](https://crates.io/crates/bit-vec/0.8.0) | 0.8.0 | Apache-2.0 OR MIT |
| [bitflags](https://crates.io/crates/bitflags/2.13.2) | 2.13.2 | MIT OR Apache-2.0 |
| [block-buffer](https://crates.io/crates/block-buffer/0.10.4) | 0.10.4 | MIT OR Apache-2.0 |
| [block-buffer](https://crates.io/crates/block-buffer/0.12.1) | 0.12.1 | MIT OR Apache-2.0 |
| [block-padding](https://crates.io/crates/block-padding/0.3.3) | 0.3.3 | MIT OR Apache-2.0 |
| [borrow-or-share](https://crates.io/crates/borrow-or-share/0.2.4) | 0.2.4 | MIT-0 |
| [bumpalo](https://crates.io/crates/bumpalo/3.20.3) | 3.20.3 | MIT OR Apache-2.0 |
| [bytecount](https://crates.io/crates/bytecount/0.6.9) | 0.6.9 | Apache-2.0/MIT |
| [byteorder](https://crates.io/crates/byteorder/1.5.0) | 1.5.0 | Unlicense OR MIT |
| [bytes](https://crates.io/crates/bytes/1.12.2) | 1.12.2 | MIT |
| [cbc](https://crates.io/crates/cbc/0.1.2) | 0.1.2 | MIT OR Apache-2.0 |
| [cc](https://crates.io/crates/cc/1.11.2) | 1.11.2 | MIT OR Apache-2.0 |
| [cfg-if](https://crates.io/crates/cfg-if/1.0.5) | 1.0.5 | MIT OR Apache-2.0 |
| [cfg_aliases](https://crates.io/crates/cfg_aliases/0.2.2) | 0.2.2 | MIT |
| [chacha20](https://crates.io/crates/chacha20/0.10.2) | 0.10.2 | MIT OR Apache-2.0 |
| [cipher](https://crates.io/crates/cipher/0.4.4) | 0.4.4 | MIT OR Apache-2.0 |
| [cmake](https://crates.io/crates/cmake/0.1.58) | 0.1.58 | MIT OR Apache-2.0 |
| [cmov](https://crates.io/crates/cmov/0.5.4) | 0.5.4 | Apache-2.0 OR MIT |
| [combine](https://crates.io/crates/combine/4.6.9) | 4.6.9 | MIT |
| [const-oid](https://crates.io/crates/const-oid/0.9.6) | 0.9.6 | Apache-2.0 OR MIT |
| [core-foundation](https://crates.io/crates/core-foundation/0.10.1) | 0.10.1 | MIT OR Apache-2.0 |
| [core-foundation-sys](https://crates.io/crates/core-foundation-sys/0.8.7) | 0.8.7 | MIT OR Apache-2.0 |
| [cpufeatures](https://crates.io/crates/cpufeatures/0.2.17) | 0.2.17 | MIT OR Apache-2.0 |
| [cpufeatures](https://crates.io/crates/cpufeatures/0.3.1) | 0.3.1 | MIT OR Apache-2.0 |
| [crc](https://crates.io/crates/crc/3.4.0) | 3.4.0 | MIT OR Apache-2.0 |
| [crc-catalog](https://crates.io/crates/crc-catalog/2.5.0) | 2.5.0 | MIT OR Apache-2.0 |
| [crossbeam-queue](https://crates.io/crates/crossbeam-queue/0.3.14) | 0.3.14 | MIT OR Apache-2.0 |
| [crossbeam-utils](https://crates.io/crates/crossbeam-utils/0.8.23) | 0.8.23 | MIT OR Apache-2.0 |
| [crypto-common](https://crates.io/crates/crypto-common/0.1.6) | 0.1.6 | MIT OR Apache-2.0 |
| [crypto-common](https://crates.io/crates/crypto-common/0.2.2) | 0.2.2 | MIT OR Apache-2.0 |
| [ctutils](https://crates.io/crates/ctutils/0.4.8) | 0.4.8 | Apache-2.0 OR MIT |
| [data-encoding](https://crates.io/crates/data-encoding/2.11.1) | 2.11.1 | MIT |
| [der](https://crates.io/crates/der/0.7.10) | 0.7.10 | Apache-2.0 OR MIT |
| [deranged](https://crates.io/crates/deranged/0.5.8) | 0.5.8 | MIT OR Apache-2.0 |
| [digest](https://crates.io/crates/digest/0.10.7) | 0.10.7 | MIT OR Apache-2.0 |
| [digest](https://crates.io/crates/digest/0.11.3) | 0.11.3 | MIT OR Apache-2.0 |
| [displaydoc](https://crates.io/crates/displaydoc/0.2.7) | 0.2.7 | MIT OR Apache-2.0 |
| [dotenvy](https://crates.io/crates/dotenvy/0.15.7) | 0.15.7 | MIT |
| [dunce](https://crates.io/crates/dunce/1.0.5) | 1.0.5 | CC0-1.0 OR MIT-0 OR Apache-2.0 |
| [dyn-clone](https://crates.io/crates/dyn-clone/1.0.20) | 1.0.20 | MIT OR Apache-2.0 |
| [either](https://crates.io/crates/either/1.19.0) | 1.19.0 | MIT OR Apache-2.0 |
| [email_address](https://crates.io/crates/email_address/0.2.10) | 0.2.10 | MIT OR Apache-2.0 |
| [equivalent](https://crates.io/crates/equivalent/1.0.3) | 1.0.3 | Apache-2.0 OR MIT |
| [errno](https://crates.io/crates/errno/0.3.15) | 0.3.15 | MIT OR Apache-2.0 |
| [etcetera](https://crates.io/crates/etcetera/0.11.0) | 0.11.0 | MIT OR Apache-2.0 |
| [event-listener](https://crates.io/crates/event-listener/5.4.2) | 5.4.2 | Apache-2.0 OR MIT |
| [fancy-regex](https://crates.io/crates/fancy-regex/0.16.2) | 0.16.2 | MIT |
| [fastrand](https://crates.io/crates/fastrand/2.5.0) | 2.5.0 | Apache-2.0 OR MIT |
| [find-msvc-tools](https://crates.io/crates/find-msvc-tools/0.1.15) | 0.1.15 | MIT OR Apache-2.0 |
| [fluent-uri](https://crates.io/crates/fluent-uri/0.4.1) | 0.4.1 | MIT |
| [flume](https://crates.io/crates/flume/0.12.0) | 0.12.0 | Apache-2.0/MIT |
| [fnv](https://crates.io/crates/fnv/1.0.7) | 1.0.7 | Apache-2.0 / MIT |
| [foldhash](https://crates.io/crates/foldhash/0.2.0) | 0.2.0 | Zlib |
| [form_urlencoded](https://crates.io/crates/form_urlencoded/1.2.2) | 1.2.2 | MIT OR Apache-2.0 |
| [fraction](https://crates.io/crates/fraction/0.15.4) | 0.15.4 | MIT OR Apache-2.0 |
| [fs_extra](https://crates.io/crates/fs_extra/1.3.0) | 1.3.0 | MIT |
| [futures-channel](https://crates.io/crates/futures-channel/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-core](https://crates.io/crates/futures-core/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-executor](https://crates.io/crates/futures-executor/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-intrusive](https://crates.io/crates/futures-intrusive/0.5.0) | 0.5.0 | MIT OR Apache-2.0 |
| [futures-io](https://crates.io/crates/futures-io/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-macro](https://crates.io/crates/futures-macro/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-sink](https://crates.io/crates/futures-sink/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-task](https://crates.io/crates/futures-task/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [futures-util](https://crates.io/crates/futures-util/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [generic-array](https://crates.io/crates/generic-array/0.14.9) | 0.14.9 | MIT |
| [getrandom](https://crates.io/crates/getrandom/0.2.17) | 0.2.17 | MIT OR Apache-2.0 |
| [getrandom](https://crates.io/crates/getrandom/0.3.4) | 0.3.4 | MIT OR Apache-2.0 |
| [getrandom](https://crates.io/crates/getrandom/0.4.3) | 0.4.3 | MIT OR Apache-2.0 |
| [h2](https://crates.io/crates/h2/0.4.20) | 0.4.20 | MIT |
| [hashbrown](https://crates.io/crates/hashbrown/0.16.1) | 0.16.1 | MIT OR Apache-2.0 |
| [hashbrown](https://crates.io/crates/hashbrown/0.17.1) | 0.17.1 | MIT OR Apache-2.0 |
| [hashlink](https://crates.io/crates/hashlink/0.11.1) | 0.11.1 | MIT OR Apache-2.0 |
| [heck](https://crates.io/crates/heck/0.5.0) | 0.5.0 | MIT OR Apache-2.0 |
| [hex](https://crates.io/crates/hex/0.4.3) | 0.4.3 | MIT OR Apache-2.0 |
| [hkdf](https://crates.io/crates/hkdf/0.13.0) | 0.13.0 | MIT OR Apache-2.0 |
| [hmac](https://crates.io/crates/hmac/0.12.1) | 0.12.1 | MIT OR Apache-2.0 |
| [hmac](https://crates.io/crates/hmac/0.13.0) | 0.13.0 | MIT OR Apache-2.0 |
| [http](https://crates.io/crates/http/1.5.0) | 1.5.0 | MIT OR Apache-2.0 |
| [http-body](https://crates.io/crates/http-body/1.1.0) | 1.1.0 | MIT |
| [http-body-util](https://crates.io/crates/http-body-util/0.1.5) | 0.1.5 | MIT |
| [httparse](https://crates.io/crates/httparse/1.10.1) | 1.10.1 | MIT OR Apache-2.0 |
| [httpdate](https://crates.io/crates/httpdate/1.0.3) | 1.0.3 | MIT OR Apache-2.0 |
| [hybrid-array](https://crates.io/crates/hybrid-array/0.4.15) | 0.4.15 | MIT OR Apache-2.0 |
| [hyper](https://crates.io/crates/hyper/1.12.0) | 1.12.0 | MIT |
| [hyper-rustls](https://crates.io/crates/hyper-rustls/0.27.10) | 0.27.10 | Apache-2.0 OR ISC OR MIT |
| [hyper-util](https://crates.io/crates/hyper-util/0.1.21) | 0.1.21 | MIT |
| [icu_collections](https://crates.io/crates/icu_collections/2.4.0) | 2.4.0 | Unicode-3.0 |
| [icu_locale_core](https://crates.io/crates/icu_locale_core/2.4.0) | 2.4.0 | Unicode-3.0 |
| [icu_normalizer](https://crates.io/crates/icu_normalizer/2.4.0) | 2.4.0 | Unicode-3.0 |
| [icu_normalizer_data](https://crates.io/crates/icu_normalizer_data/2.4.0) | 2.4.0 | Unicode-3.0 |
| [icu_properties](https://crates.io/crates/icu_properties/2.4.0) | 2.4.0 | Unicode-3.0 |
| [icu_properties_data](https://crates.io/crates/icu_properties_data/2.4.0) | 2.4.0 | Unicode-3.0 |
| [icu_provider](https://crates.io/crates/icu_provider/2.4.0) | 2.4.0 | Unicode-3.0 |
| [idna](https://crates.io/crates/idna/1.1.0) | 1.1.0 | MIT OR Apache-2.0 |
| [idna_adapter](https://crates.io/crates/idna_adapter/1.2.2) | 1.2.2 | Apache-2.0 OR MIT |
| [indexmap](https://crates.io/crates/indexmap/2.14.2) | 2.14.2 | Apache-2.0 OR MIT |
| [inout](https://crates.io/crates/inout/0.1.4) | 0.1.4 | MIT OR Apache-2.0 |
| [ipnet](https://crates.io/crates/ipnet/2.12.2) | 2.12.2 | MIT OR Apache-2.0 |
| [itoa](https://crates.io/crates/itoa/1.0.19) | 1.0.19 | MIT OR Apache-2.0 |
| [jni](https://crates.io/crates/jni/0.22.4) | 0.22.4 | MIT OR Apache-2.0 |
| [jni-macros](https://crates.io/crates/jni-macros/0.22.4) | 0.22.4 | MIT OR Apache-2.0 |
| [jni-sys](https://crates.io/crates/jni-sys/0.4.1) | 0.4.1 | MIT OR Apache-2.0 |
| [jni-sys-macros](https://crates.io/crates/jni-sys-macros/0.4.1) | 0.4.1 | MIT OR Apache-2.0 |
| [jobserver](https://crates.io/crates/jobserver/0.1.35) | 0.1.35 | MIT OR Apache-2.0 |
| [js-sys](https://crates.io/crates/js-sys/0.3.103) | 0.3.103 | MIT OR Apache-2.0 |
| [jsonschema](https://crates.io/crates/jsonschema/0.37.4) | 0.37.4 | MIT |
| [lazy_static](https://crates.io/crates/lazy_static/1.5.1) | 1.5.1 | MIT OR Apache-2.0 |
| [libc](https://crates.io/crates/libc/0.2.191) | 0.2.191 | MIT OR Apache-2.0 |
| [libm](https://crates.io/crates/libm/0.2.16) | 0.2.16 | MIT |
| [libredox](https://crates.io/crates/libredox/0.1.25) | 0.1.25 | MIT |
| [libsqlite3-sys](https://crates.io/crates/libsqlite3-sys/0.40.0) | 0.40.0 | MIT |
| [linux-raw-sys](https://crates.io/crates/linux-raw-sys/0.12.1) | 0.12.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [litemap](https://crates.io/crates/litemap/0.8.3) | 0.8.3 | Unicode-3.0 |
| [lock_api](https://crates.io/crates/lock_api/0.4.14) | 0.4.14 | MIT OR Apache-2.0 |
| [log](https://crates.io/crates/log/0.4.35) | 0.4.35 | MIT OR Apache-2.0 |
| [lru-slab](https://crates.io/crates/lru-slab/0.1.3) | 0.1.3 | MIT OR Apache-2.0 OR Zlib |
| [matchers](https://crates.io/crates/matchers/0.2.0) | 0.2.0 | MIT |
| [matchit](https://crates.io/crates/matchit/0.8.4) | 0.8.4 | MIT AND BSD-3-Clause |
| [md-5](https://crates.io/crates/md-5/0.11.0) | 0.11.0 | MIT OR Apache-2.0 |
| [md5](https://crates.io/crates/md5/0.8.1) | 0.8.1 | Apache-2.0 OR MIT |
| [memchr](https://crates.io/crates/memchr/2.8.3) | 2.8.3 | Unlicense OR MIT |
| [mime](https://crates.io/crates/mime/0.3.18) | 0.3.18 | MIT OR Apache-2.0 |
| [mio](https://crates.io/crates/mio/1.2.5) | 1.2.5 | MIT |
| [nu-ansi-term](https://crates.io/crates/nu-ansi-term/0.50.3) | 0.50.3 | MIT |
| [num](https://crates.io/crates/num/0.4.3) | 0.4.3 | MIT OR Apache-2.0 |
| [num-bigint](https://crates.io/crates/num-bigint/0.4.8) | 0.4.8 | MIT OR Apache-2.0 |
| [num-bigint-dig](https://crates.io/crates/num-bigint-dig/0.8.6) | 0.8.6 | MIT/Apache-2.0 |
| [num-cmp](https://crates.io/crates/num-cmp/0.1.0) | 0.1.0 | MIT/Apache-2.0 |
| [num-complex](https://crates.io/crates/num-complex/0.4.6) | 0.4.6 | MIT OR Apache-2.0 |
| [num-conv](https://crates.io/crates/num-conv/0.2.2) | 0.2.2 | MIT OR Apache-2.0 |
| [num-integer](https://crates.io/crates/num-integer/0.1.47) | 0.1.47 | MIT OR Apache-2.0 |
| [num-iter](https://crates.io/crates/num-iter/0.1.46) | 0.1.46 | MIT OR Apache-2.0 |
| [num-rational](https://crates.io/crates/num-rational/0.4.2) | 0.4.2 | MIT OR Apache-2.0 |
| [num-traits](https://crates.io/crates/num-traits/0.2.19) | 0.2.19 | MIT OR Apache-2.0 |
| [objc2-core-foundation](https://crates.io/crates/objc2-core-foundation/0.3.2) | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| [objc2-system-configuration](https://crates.io/crates/objc2-system-configuration/0.3.2) | 0.3.2 | Zlib OR Apache-2.0 OR MIT |
| [once_cell](https://crates.io/crates/once_cell/1.21.4) | 1.21.4 | MIT OR Apache-2.0 |
| [openssl-probe](https://crates.io/crates/openssl-probe/0.2.2) | 0.2.2 | MIT OR Apache-2.0 |
| [outref](https://crates.io/crates/outref/0.5.2) | 0.5.2 | MIT |
| [parking](https://crates.io/crates/parking/2.2.1) | 2.2.1 | Apache-2.0 OR MIT |
| [parking_lot](https://crates.io/crates/parking_lot/0.12.5) | 0.12.5 | MIT OR Apache-2.0 |
| [parking_lot_core](https://crates.io/crates/parking_lot_core/0.9.12) | 0.9.12 | MIT OR Apache-2.0 |
| [pem-rfc7468](https://crates.io/crates/pem-rfc7468/0.7.0) | 0.7.0 | Apache-2.0 OR MIT |
| [percent-encoding](https://crates.io/crates/percent-encoding/2.3.2) | 2.3.2 | MIT OR Apache-2.0 |
| [pin-project-lite](https://crates.io/crates/pin-project-lite/0.2.17) | 0.2.17 | Apache-2.0 OR MIT |
| [pkcs1](https://crates.io/crates/pkcs1/0.7.5) | 0.7.5 | Apache-2.0 OR MIT |
| [pkcs8](https://crates.io/crates/pkcs8/0.10.2) | 0.10.2 | Apache-2.0 OR MIT |
| [pkg-config](https://crates.io/crates/pkg-config/0.3.34) | 0.3.34 | MIT OR Apache-2.0 |
| [potential_utf](https://crates.io/crates/potential_utf/0.1.6) | 0.1.6 | Unicode-3.0 |
| [powerfmt](https://crates.io/crates/powerfmt/0.2.0) | 0.2.0 | MIT OR Apache-2.0 |
| [ppv-lite86](https://crates.io/crates/ppv-lite86/0.2.21) | 0.2.21 | MIT OR Apache-2.0 |
| [proc-macro2](https://crates.io/crates/proc-macro2/1.0.107) | 1.0.107 | MIT OR Apache-2.0 |
| [quinn](https://crates.io/crates/quinn/0.11.12) | 0.11.12 | MIT OR Apache-2.0 |
| [quinn-proto](https://crates.io/crates/quinn-proto/0.11.19) | 0.11.19 | MIT OR Apache-2.0 |
| [quinn-udp](https://crates.io/crates/quinn-udp/0.5.16) | 0.5.16 | MIT OR Apache-2.0 |
| [quote](https://crates.io/crates/quote/1.0.47) | 1.0.47 | MIT OR Apache-2.0 |
| [r-efi](https://crates.io/crates/r-efi/5.3.0) | 5.3.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| [r-efi](https://crates.io/crates/r-efi/6.0.0) | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later |
| [rand](https://crates.io/crates/rand/0.10.4) | 0.10.4 | MIT OR Apache-2.0 |
| [rand](https://crates.io/crates/rand/0.8.8) | 0.8.8 | MIT OR Apache-2.0 |
| [rand](https://crates.io/crates/rand/0.9.5) | 0.9.5 | MIT OR Apache-2.0 |
| [rand_chacha](https://crates.io/crates/rand_chacha/0.3.1) | 0.3.1 | MIT OR Apache-2.0 |
| [rand_chacha](https://crates.io/crates/rand_chacha/0.9.0) | 0.9.0 | MIT OR Apache-2.0 |
| [rand_core](https://crates.io/crates/rand_core/0.10.1) | 0.10.1 | MIT OR Apache-2.0 |
| [rand_core](https://crates.io/crates/rand_core/0.6.4) | 0.6.4 | MIT OR Apache-2.0 |
| [rand_core](https://crates.io/crates/rand_core/0.9.5) | 0.9.5 | MIT OR Apache-2.0 |
| [rand_pcg](https://crates.io/crates/rand_pcg/0.10.2) | 0.10.2 | MIT OR Apache-2.0 |
| [redox_syscall](https://crates.io/crates/redox_syscall/0.5.18) | 0.5.18 | MIT |
| [ref-cast](https://crates.io/crates/ref-cast/1.0.27) | 1.0.27 | MIT OR Apache-2.0 |
| [ref-cast-impl](https://crates.io/crates/ref-cast-impl/1.0.27) | 1.0.27 | MIT OR Apache-2.0 |
| [referencing](https://crates.io/crates/referencing/0.37.4) | 0.37.4 | MIT |
| [regex](https://crates.io/crates/regex/1.13.2) | 1.13.2 | MIT OR Apache-2.0 |
| [regex-automata](https://crates.io/crates/regex-automata/0.4.19) | 0.4.19 | MIT OR Apache-2.0 |
| [regex-syntax](https://crates.io/crates/regex-syntax/0.8.12) | 0.8.12 | MIT OR Apache-2.0 |
| [reqwest](https://crates.io/crates/reqwest/0.13.5) | 0.13.5 | MIT OR Apache-2.0 |
| [ring](https://crates.io/crates/ring/0.17.14) | 0.17.14 | Apache-2.0 AND ISC |
| [rsa](https://crates.io/crates/rsa/0.9.10) | 0.9.10 | MIT OR Apache-2.0 |
| [rustc-hash](https://crates.io/crates/rustc-hash/2.1.3) | 2.1.3 | Apache-2.0 OR MIT |
| [rustc_version](https://crates.io/crates/rustc_version/0.4.1) | 0.4.1 | MIT OR Apache-2.0 |
| [rustix](https://crates.io/crates/rustix/1.1.6) | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [rustls](https://crates.io/crates/rustls/0.23.46) | 0.23.46 | Apache-2.0 OR ISC OR MIT |
| [rustls-native-certs](https://crates.io/crates/rustls-native-certs/0.8.5) | 0.8.5 | Apache-2.0 OR ISC OR MIT |
| [rustls-pki-types](https://crates.io/crates/rustls-pki-types/1.16.0) | 1.16.0 | MIT OR Apache-2.0 |
| [rustls-platform-verifier](https://crates.io/crates/rustls-platform-verifier/0.7.1) | 0.7.1 | MIT OR Apache-2.0 |
| [rustls-platform-verifier-android](https://crates.io/crates/rustls-platform-verifier-android/0.2.0) | 0.2.0 | MIT OR Apache-2.0 |
| [rustls-webpki](https://crates.io/crates/rustls-webpki/0.103.17) | 0.103.17 | ISC |
| [rustversion](https://crates.io/crates/rustversion/1.0.23) | 1.0.23 | MIT OR Apache-2.0 |
| [ryu](https://crates.io/crates/ryu/1.0.23) | 1.0.23 | Apache-2.0 OR BSL-1.0 |
| [same-file](https://crates.io/crates/same-file/1.0.6) | 1.0.6 | Unlicense/MIT |
| [schannel](https://crates.io/crates/schannel/0.1.30) | 0.1.30 | MIT |
| [schemars](https://crates.io/crates/schemars/1.2.3) | 1.2.3 | MIT |
| [schemars_derive](https://crates.io/crates/schemars_derive/1.2.3) | 1.2.3 | MIT |
| [scopeguard](https://crates.io/crates/scopeguard/1.2.0) | 1.2.0 | MIT OR Apache-2.0 |
| [security-framework](https://crates.io/crates/security-framework/3.7.0) | 3.7.0 | MIT OR Apache-2.0 |
| [security-framework-sys](https://crates.io/crates/security-framework-sys/2.17.0) | 2.17.0 | MIT OR Apache-2.0 |
| [semver](https://crates.io/crates/semver/1.0.28) | 1.0.28 | MIT OR Apache-2.0 |
| [serde](https://crates.io/crates/serde/1.0.229) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_core](https://crates.io/crates/serde_core/1.0.229) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_derive](https://crates.io/crates/serde_derive/1.0.229) | 1.0.229 | MIT OR Apache-2.0 |
| [serde_derive_internals](https://crates.io/crates/serde_derive_internals/0.30.0) | 0.30.0 | MIT OR Apache-2.0 |
| [serde_json](https://crates.io/crates/serde_json/1.0.154) | 1.0.154 | MIT OR Apache-2.0 |
| [serde_path_to_error](https://crates.io/crates/serde_path_to_error/0.1.20) | 0.1.20 | MIT OR Apache-2.0 |
| [serde_urlencoded](https://crates.io/crates/serde_urlencoded/0.7.1) | 0.7.1 | MIT/Apache-2.0 |
| [sha1](https://crates.io/crates/sha1/0.10.7) | 0.10.7 | MIT OR Apache-2.0 |
| [sha1](https://crates.io/crates/sha1/0.11.0) | 0.11.0 | MIT OR Apache-2.0 |
| [sha2](https://crates.io/crates/sha2/0.10.9) | 0.10.9 | MIT OR Apache-2.0 |
| [sha2](https://crates.io/crates/sha2/0.11.1) | 0.11.1 | MIT OR Apache-2.0 |
| [sharded-slab](https://crates.io/crates/sharded-slab/0.1.7) | 0.1.7 | MIT |
| [shlex](https://crates.io/crates/shlex/2.0.1) | 2.0.1 | MIT OR Apache-2.0 |
| [signal-hook-registry](https://crates.io/crates/signal-hook-registry/1.4.8) | 1.4.8 | MIT OR Apache-2.0 |
| [signature](https://crates.io/crates/signature/2.2.0) | 2.2.0 | Apache-2.0 OR MIT |
| [simd_cesu8](https://crates.io/crates/simd_cesu8/1.2.0) | 1.2.0 | Apache-2.0 OR MIT |
| [simdutf8](https://crates.io/crates/simdutf8/0.1.5) | 0.1.5 | MIT OR Apache-2.0 |
| [slab](https://crates.io/crates/slab/0.4.12) | 0.4.12 | MIT |
| [smallvec](https://crates.io/crates/smallvec/1.16.5) | 1.16.5 | MIT OR Apache-2.0 |
| [socket2](https://crates.io/crates/socket2/0.6.5) | 0.6.5 | MIT OR Apache-2.0 |
| [spin](https://crates.io/crates/spin/0.9.9) | 0.9.9 | MIT |
| [spki](https://crates.io/crates/spki/0.7.3) | 0.7.3 | Apache-2.0 OR MIT |
| [sqlx](https://crates.io/crates/sqlx/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [sqlx-core](https://crates.io/crates/sqlx-core/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [sqlx-macros](https://crates.io/crates/sqlx-macros/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [sqlx-macros-core](https://crates.io/crates/sqlx-macros-core/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [sqlx-mysql](https://crates.io/crates/sqlx-mysql/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [sqlx-postgres](https://crates.io/crates/sqlx-postgres/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [sqlx-sqlite](https://crates.io/crates/sqlx-sqlite/0.9.1) | 0.9.1 | MIT OR Apache-2.0 |
| [stable_deref_trait](https://crates.io/crates/stable_deref_trait/1.2.1) | 1.2.1 | MIT OR Apache-2.0 |
| [stringprep](https://crates.io/crates/stringprep/0.1.5) | 0.1.5 | MIT/Apache-2.0 |
| [subtle](https://crates.io/crates/subtle/2.6.1) | 2.6.1 | BSD-3-Clause |
| [syn](https://crates.io/crates/syn/2.0.119) | 2.0.119 | MIT OR Apache-2.0 |
| [syn](https://crates.io/crates/syn/3.0.10) | 3.0.10 | MIT OR Apache-2.0 |
| [sync_wrapper](https://crates.io/crates/sync_wrapper/1.0.2) | 1.0.2 | Apache-2.0 |
| [synstructure](https://crates.io/crates/synstructure/0.14.0) | 0.14.0 | MIT |
| [tempfile](https://crates.io/crates/tempfile/3.27.0) | 3.27.0 | MIT OR Apache-2.0 |
| [thiserror](https://crates.io/crates/thiserror/2.0.21) | 2.0.21 | MIT OR Apache-2.0 |
| [thiserror-impl](https://crates.io/crates/thiserror-impl/2.0.21) | 2.0.21 | MIT OR Apache-2.0 |
| [thread_local](https://crates.io/crates/thread_local/1.1.10) | 1.1.10 | MIT OR Apache-2.0 |
| [time](https://crates.io/crates/time/0.3.55) | 0.3.55 | MIT OR Apache-2.0 |
| [time-core](https://crates.io/crates/time-core/0.1.9) | 0.1.9 | MIT OR Apache-2.0 |
| [time-macros](https://crates.io/crates/time-macros/0.2.32) | 0.2.32 | MIT OR Apache-2.0 |
| [tinystr](https://crates.io/crates/tinystr/0.8.5) | 0.8.5 | Unicode-3.0 |
| [tinyvec](https://crates.io/crates/tinyvec/1.13.3) | 1.13.3 | Zlib OR Apache-2.0 OR MIT |
| [tokio](https://crates.io/crates/tokio/1.54.1) | 1.54.1 | MIT |
| [tokio-macros](https://crates.io/crates/tokio-macros/2.7.2) | 2.7.2 | MIT |
| [tokio-rustls](https://crates.io/crates/tokio-rustls/0.26.6) | 0.26.6 | MIT OR Apache-2.0 |
| [tokio-stream](https://crates.io/crates/tokio-stream/0.1.19) | 0.1.19 | MIT |
| [tokio-tungstenite](https://crates.io/crates/tokio-tungstenite/0.28.0) | 0.28.0 | MIT |
| [tokio-util](https://crates.io/crates/tokio-util/0.7.20) | 0.7.20 | MIT |
| [tower](https://crates.io/crates/tower/0.5.3) | 0.5.3 | MIT |
| [tower-http](https://crates.io/crates/tower-http/0.6.11) | 0.6.11 | MIT |
| [tower-layer](https://crates.io/crates/tower-layer/0.3.3) | 0.3.3 | MIT |
| [tower-service](https://crates.io/crates/tower-service/0.3.3) | 0.3.3 | MIT |
| [tracing](https://crates.io/crates/tracing/0.1.44) | 0.1.44 | MIT |
| [tracing-attributes](https://crates.io/crates/tracing-attributes/0.1.31) | 0.1.31 | MIT |
| [tracing-core](https://crates.io/crates/tracing-core/0.1.36) | 0.1.36 | MIT |
| [tracing-log](https://crates.io/crates/tracing-log/0.2.0) | 0.2.0 | MIT |
| [tracing-subscriber](https://crates.io/crates/tracing-subscriber/0.3.23) | 0.3.23 | MIT |
| [try-lock](https://crates.io/crates/try-lock/0.2.5) | 0.2.5 | MIT |
| [tungstenite](https://crates.io/crates/tungstenite/0.28.0) | 0.28.0 | MIT OR Apache-2.0 |
| [typenum](https://crates.io/crates/typenum/1.20.1) | 1.20.1 | MIT OR Apache-2.0 |
| [unicode-bidi](https://crates.io/crates/unicode-bidi/0.3.18) | 0.3.18 | MIT OR Apache-2.0 |
| [unicode-general-category](https://crates.io/crates/unicode-general-category/1.1.0) | 1.1.0 | Apache-2.0 |
| [unicode-ident](https://crates.io/crates/unicode-ident/1.0.27) | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| [unicode-normalization](https://crates.io/crates/unicode-normalization/0.1.26) | 0.1.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| [unicode-properties](https://crates.io/crates/unicode-properties/0.1.5) | 0.1.5 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| [untrusted](https://crates.io/crates/untrusted/0.9.0) | 0.9.0 | ISC |
| [url](https://crates.io/crates/url/2.5.8) | 2.5.8 | MIT OR Apache-2.0 |
| [utf-8](https://crates.io/crates/utf-8/0.7.6) | 0.7.6 | MIT OR Apache-2.0 |
| [utf8_iter](https://crates.io/crates/utf8_iter/1.1.0) | 1.1.0 | Apache-2.0 OR MIT |
| [utf_types](https://crates.io/crates/utf_types/1.0.0) | 1.0.0 | Apache-2.0 OR MIT |
| [uuid](https://crates.io/crates/uuid/1.29.3) | 1.29.3 | Apache-2.0 OR MIT |
| [uuid-simd](https://crates.io/crates/uuid-simd/0.8.0) | 0.8.0 | MIT |
| [valuable](https://crates.io/crates/valuable/0.1.1) | 0.1.1 | MIT |
| [vcpkg](https://crates.io/crates/vcpkg/0.2.15) | 0.2.15 | MIT/Apache-2.0 |
| [version_check](https://crates.io/crates/version_check/0.9.5) | 0.9.5 | MIT/Apache-2.0 |
| [vsimd](https://crates.io/crates/vsimd/0.8.0) | 0.8.0 | MIT |
| [walkdir](https://crates.io/crates/walkdir/2.5.0) | 2.5.0 | Unlicense/MIT |
| [want](https://crates.io/crates/want/0.3.2) | 0.3.2 | MIT |
| [wasi](https://crates.io/crates/wasi/0.11.1%2Bwasi-snapshot-preview1) | 0.11.1+wasi-snapshot-preview1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [wasip2](https://crates.io/crates/wasip2/1.0.4%2Bwasi-0.2.12) | 1.0.4+wasi-0.2.12 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [wasm-bindgen](https://crates.io/crates/wasm-bindgen/0.2.126) | 0.2.126 | MIT OR Apache-2.0 |
| [wasm-bindgen-futures](https://crates.io/crates/wasm-bindgen-futures/0.4.76) | 0.4.76 | MIT OR Apache-2.0 |
| [wasm-bindgen-macro](https://crates.io/crates/wasm-bindgen-macro/0.2.126) | 0.2.126 | MIT OR Apache-2.0 |
| [wasm-bindgen-macro-support](https://crates.io/crates/wasm-bindgen-macro-support/0.2.126) | 0.2.126 | MIT OR Apache-2.0 |
| [wasm-bindgen-shared](https://crates.io/crates/wasm-bindgen-shared/0.2.126) | 0.2.126 | MIT OR Apache-2.0 |
| [wasm-streams](https://crates.io/crates/wasm-streams/0.5.0) | 0.5.0 | MIT OR Apache-2.0 |
| [web-sys](https://crates.io/crates/web-sys/0.3.103) | 0.3.103 | MIT OR Apache-2.0 |
| [web-time](https://crates.io/crates/web-time/1.1.0) | 1.1.0 | MIT OR Apache-2.0 |
| [webpki-root-certs](https://crates.io/crates/webpki-root-certs/1.0.12) | 1.0.12 | CDLA-Permissive-2.0 |
| [whoami](https://crates.io/crates/whoami/2.1.3) | 2.1.3 | Apache-2.0 OR BSL-1.0 OR MIT |
| [winapi-util](https://crates.io/crates/winapi-util/0.1.11) | 0.1.11 | Unlicense OR MIT |
| [windows-link](https://crates.io/crates/windows-link/0.2.1) | 0.2.1 | MIT OR Apache-2.0 |
| [windows-sys](https://crates.io/crates/windows-sys/0.52.0) | 0.52.0 | MIT OR Apache-2.0 |
| [windows-sys](https://crates.io/crates/windows-sys/0.61.2) | 0.61.2 | MIT OR Apache-2.0 |
| [windows-targets](https://crates.io/crates/windows-targets/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_aarch64_gnullvm](https://crates.io/crates/windows_aarch64_gnullvm/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_aarch64_msvc](https://crates.io/crates/windows_aarch64_msvc/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_i686_gnu](https://crates.io/crates/windows_i686_gnu/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_i686_gnullvm](https://crates.io/crates/windows_i686_gnullvm/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_i686_msvc](https://crates.io/crates/windows_i686_msvc/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_x86_64_gnu](https://crates.io/crates/windows_x86_64_gnu/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_x86_64_gnullvm](https://crates.io/crates/windows_x86_64_gnullvm/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [windows_x86_64_msvc](https://crates.io/crates/windows_x86_64_msvc/0.52.6) | 0.52.6 | MIT OR Apache-2.0 |
| [wit-bindgen](https://crates.io/crates/wit-bindgen/0.57.1) | 0.57.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT |
| [writeable](https://crates.io/crates/writeable/0.6.4) | 0.6.4 | Unicode-3.0 |
| [yoke](https://crates.io/crates/yoke/0.8.4) | 0.8.4 | Unicode-3.0 |
| [yoke-derive](https://crates.io/crates/yoke-derive/0.8.4) | 0.8.4 | Unicode-3.0 |
| [zerocopy](https://crates.io/crates/zerocopy/0.8.63) | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| [zerocopy-derive](https://crates.io/crates/zerocopy-derive/0.8.63) | 0.8.63 | BSD-2-Clause OR Apache-2.0 OR MIT |
| [zerofrom](https://crates.io/crates/zerofrom/0.1.8) | 0.1.8 | Unicode-3.0 |
| [zerofrom-derive](https://crates.io/crates/zerofrom-derive/0.1.8) | 0.1.8 | Unicode-3.0 |
| [zeroize](https://crates.io/crates/zeroize/1.9.2) | 1.9.2 | Apache-2.0 OR MIT |
| [zerotrie](https://crates.io/crates/zerotrie/0.2.5) | 0.2.5 | Unicode-3.0 |
| [zerovec](https://crates.io/crates/zerovec/0.11.9) | 0.11.9 | Unicode-3.0 |
| [zerovec-derive](https://crates.io/crates/zerovec-derive/0.11.7) | 0.11.7 | Unicode-3.0 |
| [zmij](https://crates.io/crates/zmij/1.0.23) | 1.0.23 | MIT |
