use std::collections::HashMap;
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use serde_json::{Map, Value}; // JSON Map 类型
use tokio::sync::mpsc::{Sender, channel}; // Tokio MPSC channel 用于异步任务间通信
use tokio::sync::Semaphore; // 引入 Semaphore
use std::sync::Arc; // 用于共享 Semaphore
use opencc_rust::{OpenCC, DefaultConfig}; // 引入 OpenCC，用于繁简转换
use scraper::{Html, Selector}; // 引入 scraper，用于 HTML 解析
use std::env; // 引入 env模块，用于读取命令行参数

// 辅助函数：检查字符串是否是有效的 URL
fn is_url(url_str: &str) -> bool {
    url::Url::parse(url_str).is_ok()
}

// 辅助函数：从 JSON 字符串中提取摘要信息
// 参数:
// - json_str: 从网络获取的 JSON 字符串
// - json_url: 获取此 JSON 的 URL，主要用于日志记录
// - key_url: DBPedia 实体的主 URL (例如 "http://dbpedia.org/resource/Example")，用于在 JSON 中定位实体数据
// - value_id: 实体的 ID，主要用于日志记录
// 返回:
// - 一个 Map，键是语言代码 ("en", "zh")，值是对应的摘要文本 (serde_json::Value::String)
// - 如果解析失败或未找到有效摘要，则返回空的 Map
async fn extract_abstracts_from_json(
    json_str: &str,
    json_url: &str, // 用于日志
    key_url: &str,  // DBPedia 实体的主 URL
    value_id: u64,  // 主要用于日志
) -> Map<String, Value> {
    let mut extracted_data_map: Map<String, Value> = Map::new();

    // 如果 JSON 字符串为空，直接返回空 Map
    if json_str.is_empty() {
        // eprintln!("URL {} (ID: {}) 的 JSON 字符串为空 (extract_abstracts_from_json)", json_url, value_id); // 如有需要，可在 task 函数中记录此日志
        return extracted_data_map;
    }

    // 解析 JSON 字符串
    let root_val: Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("URL {} (ID: {}) 解析 JSON 失败: {}", json_url, value_id, e);
            return extracted_data_map; // 解析失败则返回空 Map
        }
    };

    // 安全地导航 JSON 结构:
    // 期望的结构类似 root_val["http://dbpedia.org/resource/Example"]["http://dbpedia.org/ontology/abstract"]
    if let Some(entity_obj) = root_val.get(key_url).and_then(Value::as_object) {
        if let Some(abstract_array) = entity_obj
            .get("http://dbpedia.org/ontology/abstract") // 获取摘要数组
            .and_then(Value::as_array)
        {
            for abstract_item in abstract_array {
                if let Some(item_obj) = abstract_item.as_object() {
                    // 提取语言 (lang) 和值 (value)
                    let lang = item_obj
                        .get("lang")
                        .and_then(Value::as_str)
                        .map(|s| s.to_lowercase()) // 将语言代码转为小写
                        .unwrap_or_else(|| "und".to_string()); // 为缺失或类型错误的 lang 提供默认值 "und" (undetermined)

                    // 只处理英文 (en) 和中文 (zh) 摘要
                    if lang == "en" || lang == "zh" {
                        if let Some(value_str) = item_obj.get("value").and_then(Value::as_str) {
                            let mut content_text = value_str.to_string();

                            // 如果是中文，进行繁转简
                            if lang == "zh" {
                                match OpenCC::new(DefaultConfig::T2S) { // T2S 表示繁体到简体
                                    Ok(opencc) => {
                                        content_text = opencc.convert(&content_text);
                                    }
                                    Err(e) => {
                                        eprintln!(
                                            "URL {} (ID: {}) 初始化 OpenCC T2S (JSON处理) 失败: {}. 将使用原文。",
                                            json_url, value_id, e
                                        );
                                    }
                                }
                            }

                            // 如果内容不为空，则存入 extracted_data_map
                            if !content_text.is_empty() {
                                extracted_data_map.insert(lang, Value::String(content_text));
                            }
                        }
                    }
                }
            }
        } else {
            // 这种情况可能比较常见，例如实体存在但没有 abstract 字段
            // eprintln!("URL {} (ID: {}) 在 JSON 中未找到 'http://dbpedia.org/ontology/abstract' 数组或该字段不是数组。", json_url, value_id);
        }
    } else {
        eprintln!("URL {} (ID: {}) 在 JSON 中未找到实体键 '{}' 或该键对应的值不是一个对象。", json_url, value_id, key_url);
    }

    // 如果输入 JSON 不为空，但最终没有提取到任何数据，可以打印一条日志
    if extracted_data_map.is_empty() && !json_str.is_empty() {
        println!("URL {} (ID: {}) 未从 JSON 中提取到 'en' 或 'zh' 的摘要内容。", json_url, value_id);
    }

    extracted_data_map
}

// 辅助函数：从 HTML 页面提取摘要信息 (二次检查)
// 参数:
// - client: HTTP 客户端 (借用)
// - html_url: DBPedia 资源页面的 URL (例如 "http://dbpedia.org/resource/Example")
// - value_id: 实体的 ID，主要用于日志记录
// 返回:
// - 一个 Map，键是语言代码 ("en", "zh")，值是对应的摘要文本
// - 如果抓取或解析失败，或未找到有效摘要，则返回空的 Map
async fn twice_check(
    client: &reqwest::Client, // HTTP 客户端 (借用)
    html_url: &str,        // DBPedia 资源页面的 URL
    value_id: u64,         // 实体的 ID，主要用于日志记录
) -> Map<String, Value> {
    let mut extracted_data_map: Map<String, Value> = Map::new();
    println!("HTML 抓取: 尝试 URL {} (ID: {})", html_url, value_id);

    let mut attempts = 0; // 当前尝试次数
    let max_attempts = 3; // HTML 抓取的最大尝试次数

    // 循环尝试获取 HTML 内容
    let html_body = loop {
        if attempts >= max_attempts {
            eprintln!("HTML 抓取: URL {} (ID: {}) 在 {} 次尝试后抓取 HTML 失败。", html_url, value_id, max_attempts);
            return extracted_data_map; // 失败则返回空 Map
        }
        attempts += 1;

        // reqwest 客户端默认会跟随重定向，这对于从 /resource/ 到 /page/ 的跳转很有用
        let response_result = tokio::time::timeout(
            std::time::Duration::from_secs(15), // HTML 页面抓取超时时间（可以设置得比 JSON 长一些）
            client.get(html_url).send()
        ).await;

        match response_result {
            Ok(Ok(response)) => { // 请求成功发送并收到响应
                if response.status().is_success() { // HTTP 状态码表示成功
                    match response.text().await { // 尝试获取响应体文本
                        Ok(text) => break Some(text), // 成功获取 HTML，跳出循环
                        Err(e) => {
                            eprintln!("HTML 抓取尝试 {}: URL {} (ID: {}) 获取 HTML 响应文本失败: {}", attempts, html_url, value_id, e);
                        }
                    }
                } else {
                    eprintln!("HTML 抓取尝试 {}: URL {} (ID: {}) HTML 请求失败，状态码: {}", attempts, html_url, value_id, response.status());
                }
            }
            Ok(Err(e)) => { // reqwest 库的错误 (例如网络问题)
                eprintln!("HTML 抓取尝试 {}: URL {} (ID: {}) 发送 HTML 请求失败: {}", attempts, html_url, value_id, e);
            }
            Err(_) => { // Tokio timeout 错误
                eprintln!("HTML 抓取尝试 {}: URL {} (ID: {}) HTML 请求超时。", attempts, html_url, value_id);
            }
        }
        // 发生错误或超时后，等待一段时间再重试（退避策略）
        tokio::time::sleep(std::time::Duration::from_secs(attempts as u64 * 2)).await;
    };

    // 检查 html_body 是否为 None (理论上如果 max_attempts > 0，由于循环结构，不应到达这里)
    if html_body.is_none() {
        eprintln!("HTML 抓取: URL {} (ID: {}) 经过多次尝试后 HTML body 仍为 None。", html_url, value_id);
        return extracted_data_map;
    }
    let body = html_body.unwrap(); // 解包 Option<String>
    if body.is_empty() {
        println!("HTML 抓取: URL {} (ID: {}) 抓取到的 HTML body 为空。", html_url, value_id);
        return extracted_data_map;
    }

    // 解析 HTML
    let document = Html::parse_document(&body);

    // 用于匹配 <span property="dbo:abstract" lang="..."> 的选择器
    let selector_abstract_span = match Selector::parse(r#"span[property="dbo:abstract"]"#) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("HTML 抓取: URL {} (ID: {}) 解析选择器 'span[property=\"dbo:abstract\"]' 失败: {}. 跳过 HTML 摘要提取。", html_url, value_id, e);
            return extracted_data_map;
        }
    };

    for element in document.select(&selector_abstract_span) {
        // 从 'lang' 属性提取语言代码
        let lang = element.value().attr("lang")
            .map(|s| s.to_lowercase()) // 转为小写
            .unwrap_or_else(|| "und".to_string()); // 默认为 "und" (undetermined)

        // 只处理英文 (en) 和中文 (zh) 摘要
        if lang == "en" || lang == "zh" {
            // 收集 span 标签内的所有文本节点，并去除首尾空格
            let mut content_text = element.text().collect::<String>().trim().to_string();

            if content_text.is_empty() {
                continue; // 如果没有文本内容，则跳过
            }

            // 如果是中文，进行繁转简
            if lang == "zh" {
                match OpenCC::new(DefaultConfig::T2S) {
                    Ok(opencc) => {
                        content_text = opencc.convert(&content_text);
                    }
                    Err(e) => {
                        eprintln!(
                            "HTML 抓取: URL {} (ID: {}) 初始化 OpenCC T2S (HTML处理) 失败: {}. 将使用原文。",
                            html_url, value_id, e
                        );
                    }
                }
            }

            // 如果内容不为空，并且该语言的摘要尚未提取，则存入
            if !content_text.is_empty() && !extracted_data_map.contains_key(&lang) {
                extracted_data_map.insert(lang.clone(), Value::String(content_text));
            }
        }
    }

    // 后备方案：DBPedia 页面有时会将主要摘要（通常是英文）放在 <p class="lead"> 标签中
    // 这个标签可能没有 lang 属性。
    // 仅当通过 span 选择器未能提取到英文摘要时，才尝试此方法。
    if !extracted_data_map.contains_key("en") {
        let selector_lead_paragraph = match Selector::parse(r#"p.lead"#) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("HTML 抓取: URL {} (ID: {}) 解析选择器 'p.lead' 失败: {}.", html_url, value_id, e);
                // 此处不直接返回，因为可能已经从之前的 span 选择器中找到了中文摘要
                return extracted_data_map; // 或者根据需求决定是否继续（如果已找到中文）
            }
        };
        // 获取第一个匹配的 <p class="lead"> 元素
        if let Some(element) = document.select(&selector_lead_paragraph).next() {
            let content_text = element.text().collect::<String>().trim().to_string();
            if !content_text.is_empty() {
                println!("HTML 抓取: URL {} (ID: {}) 在 'p.lead' 中找到英文摘要。", html_url, value_id);
                extracted_data_map.insert("en".to_string(), Value::String(content_text));
            }
        }
    }


    if extracted_data_map.is_empty() {
        println!("HTML 抓取: URL {} (ID: {}) 未从 HTML 中提取到 'en' 或 'zh' 的摘要内容。", html_url, value_id);
    }

    extracted_data_map
}


// 异步任务函数：抓取网页内容、解析并发送数据
async fn task(
    client: reqwest::Client, // HTTP 客户端
    tx: Sender<(u64, Map<String, Value>)>, // MPSC 发送端
    key_url: String, // 要抓取的原始 URL (来自 entity2id.json, 例如 "http://dbpedia.org/resource/Example")
    value_id: u64, // 对应的任务 ID
) {
    // 基础 URL 校验，并检查是否为 dbpedia.org 的链接
    if !is_url(&key_url) || !key_url.contains("dbpedia.org") {
        eprintln!("URL {} (ID: {}) 无效或不是 dbpedia.org 的 URL。跳过。", key_url, value_id);
        // 发送一个空 map，以表示此 ID 已处理（即使没有有效数据）
        // 这样写入器任务就不会永远等待一个从未发送的数据
        if let Err(e) = tx.send((value_id, Map::new())).await {
            eprintln!("URL {} (ID: {}) 为无效 URL 发送空数据失败: {}", key_url, value_id, e);
        }
        return;
    }

    // 从原始 URL 中提取资源名，并构建 DBPedia Data URL (通常以 .json 结尾)
    // 例如: http://dbpedia.org/resource/Rust_(programming_language) -> Rust_(programming_language)
    let resource_name = match key_url.rfind('/') {
        Some(idx) => &key_url[idx + 1..],
        None => {
            eprintln!("URL {} (ID: {}) 无法提取资源名。跳过。", key_url, value_id);
            if let Err(e) = tx.send((value_id, Map::new())).await {
                eprintln!("URL {} (ID: {}) 为无资源名 URL 发送空数据失败: {}", key_url, value_id, e);
            }
            return;
        }
    };
    // 构建 JSON 数据的 URL，例如 https://dbpedia.org/data/Rust_(programming_language).json
    let json_url = format!("https://dbpedia.org/data/{}.json", resource_name);

    let mut attempts = 0; // 当前尝试次数
    let max_attempts = 3; // 最大尝试次数
    let json_response_text: String; // 用于存储 JSON 响应文本

    // 循环尝试获取 JSON 内容，直到成功或达到最大尝试次数
    loop {
        if attempts >= max_attempts {
            eprintln!("JSON 抓取: URL: {} (ID: {}) 在 {} 次尝试后抓取失败。", json_url, value_id, max_attempts);
            // 同样，发送空 map 表示处理完毕
            if let Err(e) = tx.send((value_id, Map::new())).await {
                eprintln!("URL {} (ID: {}) 在 JSON 抓取达到最大尝试次数后发送空数据失败: {}", json_url, value_id, e);
            }
            return;
        }
        attempts += 1;

        // 设置请求超时（例如10秒）
        let response_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.get(&json_url).send()
        ).await;

        match response_result {
            Ok(Ok(response)) => { // 请求成功发送并收到响应
                if response.status().is_success() { // HTTP 状态码表示成功
                    match response.text().await { // 尝试获取响应体文本
                        Ok(text) => {
                            json_response_text = text; // 存储成功的响应文本
                            break; // 成功获取文本，跳出循环
                        }
                        Err(e) => {
                            eprintln!("JSON 抓取尝试 {}: URL {} (ID: {}) 获取响应文本失败: {}", attempts, json_url, value_id, e);
                        }
                    }
                } else {
                    eprintln!("JSON 抓取尝试 {}: URL {} (ID: {}) 请求失败，状态码: {}", attempts, json_url, value_id, response.status());
                }
            }
            Ok(Err(e)) => { // reqwest 库的错误 (例如网络问题)
                eprintln!("JSON 抓取尝试 {}: URL {} (ID: {}) 发送请求失败: {}", attempts, json_url, value_id, e);
            }
            Err(_) => { // Tokio timeout 错误
                eprintln!("JSON 抓取尝试 {}: URL {} (ID: {}) 请求超时。", attempts, json_url, value_id);
            }
        }
        // 发生错误或超时后，等待一段时间再重试（退避策略）
        tokio::time::sleep(std::time::Duration::from_secs(attempts as u64 * 2)).await;
    }

    // 第一次尝试：从 JSON 中提取数据
    // 注意：这里的 key_url 传给 extract_abstracts_from_json 是原始的实体 URL (http://dbpedia.org/resource/...)
    // 因为 DBPedia 的 JSON 数据通常以原始资源 URL 作为顶级键。
    let mut extracted_data = extract_abstracts_from_json(&json_response_text, &json_url, &key_url, value_id).await;

    // 如果 JSON 解析未能提取到摘要，并且 JSON 响应字符串本身并非为空，
    // 则尝试 HTML 抓取作为后备方案。
    if extracted_data.is_empty() && !json_response_text.is_empty() {
        println!("URL {} (ID: {}) JSON 解析未提取到摘要。启动 HTML 抓取作为后备方案。", key_url, value_id);
        // `key_url` 是 DBPedia 资源 URL (例如 http://dbpedia.org/resource/...)，适用于 HTML 抓取。
        // `client` 是此任务的 reqwest::Client 实例。
        extracted_data = twice_check(&client, &key_url, value_id).await;

        if extracted_data.is_empty() {
            println!("URL {} (ID: {}) 后备 HTML 抓取也未能提取到 'en' 或 'zh' 摘要。", key_url, value_id);
        } else {
            println!("URL {} (ID: {}) 成功从后备 HTML 抓取中为 {} 提取到摘要。", key_url, value_id, key_url);
        }
    } else if extracted_data.is_empty() && json_response_text.is_empty() {
        // 这种情况意味着 JSON 抓取成功，但返回了一个空的主体。
        // extract_abstracts_from_json 应该已经通过返回一个空 map 处理了这种情况。
        println!("URL {} (ID: {}) JSON 响应为空，未提取到摘要。跳过 HTML 抓取。", json_url, value_id);
    }


    // 通过 channel 发送 (ID, 提取到的数据Map)
    if let Err(e) = tx.send((value_id, extracted_data)).await {
        eprintln!("URL {} (ID: {}) 发送数据到 channel 失败: {}", key_url, value_id, e);
    }
}


// Tokio main 函数，用于异步执行
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 从命令行参数获取信号量限制
    let args: Vec<String> = env::args().collect();
    let semaphore_limit = if args.len() > 1 {
        match args[1].parse::<usize>() {
            Ok(limit) if limit > 0 => limit, // 必须是正整数
            Ok(_) => { // 处理解析成功但为0的情况
                eprintln!("信号量限制必须是正整数。使用默认值 10。");
                10 // 默认值
            }
            Err(_) => {
                eprintln!("无法将参数 '{}' 解析为有效的信号量限制 (正整数)。使用默认值 10。", args[1]);
                10 // 默认值
            }
        }
    } else {
        eprintln!("未提供信号量参数。使用默认值 10。格式: ./<程序名> <信号量数量>");
        10 // 默认值
    };
    println!("程序启动，并发任务限制 (信号量): {}", semaphore_limit);


    // 创建一个 MPSC channel，缓冲区大小为 200
    // tx 用于发送数据，rx 用于接收数据
    let (tx, mut rx) = channel::<(u64, Map<String, Value>)>(200);

    // 创建输出文件 item_texts.json
    let file = File::create("item_texts.json")?;
    let mut writer = BufWriter::new(file); // 使用 BufWriter 提高写入效率

    // 派生一个异步任务专门用于写入文件
    let writer_handle = tokio::spawn(async move {
        writer.write_all(b"{")?; // 写入 JSON 对象的起始符 '{'
        let mut first_item = true; // 标记是否是第一个条目，用于处理逗号

        // 循环接收来自其他任务的数据
        while let Some((id, data_map)) = rx.recv().await {
            if !first_item {
                writer.write_all(b",")?; // 非第一个元素前加逗号
            }
            first_item = false;

            // 将 u64类型的id 转换为字符串，并序列化为 JSON 字符串键
            // 例如，ID 123 变为字符串 "123"
            let json_key = serde_json::to_string(&id.to_string())?;
            writer.write_all(json_key.as_bytes())?;
            writer.write_all(b":")?; // 写入键值对之间的冒号

            // 使用 serde_json 将数据 (data_map) 序列化并写入 writer
            serde_json::to_writer(&mut writer, &data_map)?;
        }

        writer.write_all(b"}")?; // 写入 JSON 对象的结束符 '}'
        writer.flush()?; // 确保所有缓冲数据都写入文件
        Ok::<(), Box<dyn Error + Send + Sync>>(()) // 显式指定 Ok 的类型，用于错误处理
    });

    // 打开包含实体ID和URL的JSON文件
    let file = File::open("entity2id.json").expect("无法打开文件 entity2id.json");
    let reader = BufReader::new(file); // 使用 BufReader 提高读取效率

    // 解析 JSON 文件到 HashMap<String, u64>
    // 假设 entity2id.json 的格式是 {"<http://dbpedia.org/resource/Example>": 123, ...}
    let data: HashMap<String, u64> = serde_json::from_reader(reader)?;
    let client = reqwest::Client::new(); // 创建一个 reqwest HTTP 客户端
    println!("成功解析了 entity2id.json 文件！共 {} 条目。", data.len());

    // 使用从命令行参数获取的 semaphore_limit 创建信号量
    let semaphore = Arc::new(Semaphore::new(semaphore_limit));
    let mut handles = Vec::new(); // 用于存储所有任务的句柄

    // 遍历解析到的数据
    for (key_from_json, entity_id) in data.into_iter() { // 处理所有条目
        // entity2id.json 中的 DBPedia URL 通常格式为 "<http://dbpedia.org/resource/Example>"
        // 我们需要移除尖括号。
        let url_to_process = if key_from_json.starts_with('<') && key_from_json.ends_with('>') {
            key_from_json[1..key_from_json.len() - 1].to_string()
        } else {
            key_from_json // 如果没有尖括号，则按原样使用
        };


        // 为每个任务克隆 tx (Sender)、client 和 semaphore
        let tx_clone = tx.clone();
        let client_clone = client.clone();
        let semaphore_clone = Arc::clone(&semaphore);

        // 派生一个新的异步任务来处理每个 URL
        let handle = tokio::spawn(async move {
            // 在执行任务之前，尝试获取一个信号量许可
            // 这会阻塞直到有许可可用
            let permit = semaphore_clone.acquire_owned().await.expect("信号量获取失败");

            // 执行实际的任务逻辑
            task(client_clone, tx_clone, url_to_process, entity_id).await;

            // 当 permit 超出作用域时，许可会自动释放
            drop(permit);
        });
        handles.push(handle); // 将任务句柄添加到 Vec 中
    }

    // ★ 重要: 关闭原始的 tx。
    // 当所有克隆的 tx 和这个原始的 tx 都被 drop 后，
    // 写入任务 (writer_handle) 中的 rx.recv().await 才会返回 None，从而结束写入循环。
    drop(tx);

    // 等待所有抓取和解析任务完成
    for handle in handles {
        handle.await.expect("任务 join 失败");
    }

    // 等待写入任务完成
    writer_handle.await.expect("写入任务 join 失败").expect("写入任务执行时 panic");

    println!("处理完成。item_texts.json 文件已创建。");
    Ok(())
}