use std::process::Command;

fn main() {
    // let out_dir = format!("{}/protos", std::env::var("OUT_DIR").unwrap());

    // std::fs::create_dir_all(&out_dir).unwrap();

    let out_dir = "src/protos";

    protobuf_codegen::Codegen::new() // 创建代码生成器
        .pure() // 使用rust的，不依赖protoc
        .out_dir(out_dir)
        .inputs(["protos/rendezvous.proto", "protos/message.proto"]) // protobuf 协议文件
        .include("protos") // 设置 protos 目录作为包含 .proto 文件的文件夹
        .customize(protobuf_codegen::Customize::default().tokio_bytes(true)) // 支持 tokio::Bytes 类型
        .run()
        .expect("Codegen failed.");
    Command::new("cargo")
        .args(["fmt"])
        .current_dir("protos")
        .output()
        .unwrap();
    println!("cargo:rerun-if-changed=protos/*.proto");
}
