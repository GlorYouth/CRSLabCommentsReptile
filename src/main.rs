use scraper::{Html, Selector}; // 引入 scraper，用于 HTML 解析
use serde_json::Value; // JSON Map 类型
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::sync::Arc; // 用于共享 Semaphore
use tokio::sync::Semaphore; // 引入 Semaphore
use tokio::sync::mpsc::{Sender, channel}; // Tokio MPSC channel 用于异步任务间通信 // 引入 env模块，用于读取命令行参数

// 辅助函数：检查字符串是否是有效的 URL
fn is_url(url_str: &str) -> bool {
    url::Url::parse(url_str).is_ok()
}

fn get_json_url(resource_name: &str) -> String {
    format!("https://dbpedia.org/data/{}.json", resource_name)
}

fn get_abstract(json_str: &str,resource_name: &str) -> Option<String> {
    // 解析 JSON 字符串
    let _v = serde_json::from_str::<Value>(json_str);
    let root = match &_v {
        Ok(v) => v.as_object()?,
        Err(e) => {
            let json_url = get_json_url(resource_name);
            eprintln!("URL {} 解析 JSON 失败: {}", json_url, e);
            return Some(String::default()); // 解析失败则返回空 Map
        }
    };

    // 安全地导航 JSON 结构:
    // 期望的结构类似 root["http://dbpedia.org/resource/resource_name"]["http://dbpedia.org/ontology/abstract"]
    let r#abstract = root.iter().find_map(|(key, val)| {
        if key.ends_with(resource_name) {
            val.as_object()
        } else {
            None
        }
    }).and_then(|resource_map| {
        resource_map.iter().find_map(|(key, val)| {
            if key.ends_with("abstract") {
                val.as_array()
            } else {
                None
            }
        })
    }).and_then(|arr| {
        arr.iter().find_map(|r#abstract| {
            r#abstract.as_object().and_then(|map| {
                if map.get("lang")
                    .and_then(Value::as_str)
                    .map(|s| s.to_lowercase()) // 将语言代码转为小写
                    .unwrap_or_else(|| "und".to_string())
                    .eq("en")
                {
                    map.get("value").and_then(Value::as_str)
                } else {
                    None
                }
            })
        })
    });
    if let Some(v) = r#abstract {
        Some(v.to_owned())
    } else { 
        None
    }
}

// 辅助函数：从 JSON 字符串中提取摘要信息
// 参数:
// - json_str: 从网络获取的 JSON 字符串
// - resource_name: url关键资源名
// - value_id: 实体的 ID，主要用于日志记录
// 返回:
// - 一个 String，值是对应的摘要文本 (String)
// - 如果解析失败或未找到有效摘要，则返回None
async fn extract_abstract_from_json(
    json_str: &str,
    resource_name: &str,
    value_id: u64, // 主要用于日志
) -> String {
    // 如果 JSON 字符串为空，直接返回空 Map
    if json_str.is_empty() {
        // eprintln!("URL {} (ID: {}) 的 JSON 字符串为空 (extract_abstracts_from_json)", json_url, value_id); // 如有需要，可在 task 函数中记录此日志
        return String::default();
    }
    match get_abstract(json_str, resource_name) {
        None => {
            let json_url = get_json_url(resource_name);
            let key_url = format!("http://dbpedia.org/resource/{}", resource_name);
            eprintln!(
                "URL {} (ID: {}) 在 JSON 中未找到实体键 '{}' 或该键对应的值不是一个对象。",
                json_url, value_id, key_url
            );
        }
        Some(str) => {
            return str
        }
    }
    let json_url = get_json_url(resource_name);
    println!(
        "URL {} (ID: {}) 未从 JSON 中提取到 'en' 的摘要内容。",
        json_url, value_id
    );
    String::default()
}

// 辅助函数：从 HTML 页面提取摘要信息 (二次检查)
// 参数:
// - client: HTTP 客户端 (借用)
// - html_url: DBPedia 资源页面的 URL (例如 "http://dbpedia.org/resource/Example")
// - value_id: 实体的 ID，主要用于日志记录
// 返回:
// - 一个 String 值是对应的'en'摘要文本
// - 如果抓取或解析失败，或未找到有效摘要，则返回String::default()
async fn twice_check(
    client: &reqwest::Client, // HTTP 客户端 (借用)
    html_url: &str,           // DBPedia 资源页面的 URL
    value_id: u64,            // 实体的 ID，主要用于日志记录
) -> String {
    println!("HTML 抓取: 尝试 URL {} (ID: {})", html_url, value_id);

    let mut attempts = 0; // 当前尝试次数
    let max_attempts = 3; // HTML 抓取的最大尝试次数

    // 循环尝试获取 HTML 内容
    let html_body = loop {
        if attempts >= max_attempts {
            eprintln!(
                "HTML 抓取: URL {} (ID: {}) 在 {} 次尝试后抓取 HTML 失败。",
                html_url, value_id, max_attempts
            );
            return String::default(); // 失败则返回空 Map
        }
        attempts += 1;

        // reqwest 客户端默认会跟随重定向，这对于从 /resource/ 到 /page/ 的跳转很有用
        let response_result = tokio::time::timeout(
            std::time::Duration::from_secs(15), // HTML 页面抓取超时时间（可以设置得比 JSON 长一些）
            client.get(html_url).send(),
        )
        .await;

        match response_result {
            Ok(Ok(response)) => {
                // 请求成功发送并收到响应
                if response.status().is_success() {
                    // HTTP 状态码表示成功
                    match response.text().await {
                        // 尝试获取响应体文本
                        Ok(text) => break Some(text), // 成功获取 HTML，跳出循环
                        Err(e) => {
                            eprintln!(
                                "HTML 抓取尝试 {}: URL {} (ID: {}) 获取 HTML 响应文本失败: {}",
                                attempts, html_url, value_id, e
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "HTML 抓取尝试 {}: URL {} (ID: {}) HTML 请求失败，状态码: {}",
                        attempts,
                        html_url,
                        value_id,
                        response.status()
                    );
                }
            }
            Ok(Err(e)) => {
                // reqwest 库的错误 (例如网络问题)
                eprintln!(
                    "HTML 抓取尝试 {}: URL {} (ID: {}) 发送 HTML 请求失败: {}",
                    attempts, html_url, value_id, e
                );
            }
            Err(_) => {
                // Tokio timeout 错误
                eprintln!(
                    "HTML 抓取尝试 {}: URL {} (ID: {}) HTML 请求超时。",
                    attempts, html_url, value_id
                );
            }
        }
        // 发生错误或超时后，等待一段时间再重试（退避策略）
        tokio::time::sleep(std::time::Duration::from_secs(attempts as u64 * 2)).await;
    };

    // 检查 html_body 是否为 String::default() (理论上如果 max_attempts > 0，由于循环结构，不应到达这里)
    if html_body.is_none() {
        eprintln!(
            "HTML 抓取: URL {} (ID: {}) 经过多次尝试后 HTML body 仍为空。",
            html_url, value_id
        );
        return String::default();
    }
    let body = html_body.unwrap(); // 解包 Option<String>
    if body.is_empty() {
        println!(
            "HTML 抓取: URL {} (ID: {}) 抓取到的 HTML body 为空。",
            html_url, value_id
        );
        return String::default();
    }

    // 解析 HTML
    let document = Html::parse_document(&body);

    // 用于匹配 <span property="dbo:abstract" lang="en"> 的选择器
    let selector_abstract_span = match Selector::parse(r#"span[property="dbo:abstract"][lang="en"]"#) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "HTML 抓取: URL {} (ID: {}) 解析选择器 'span[property=\"dbo:abstract\"[lang=\"en\"]]' 失败: {}. 跳过 HTML 摘要提取。",
                html_url, value_id, e
            );
            return String::default();
        }
    };

    for element in document.select(&selector_abstract_span) {
        // 从 'lang' 属性提取语言代码
        // 收集 span 标签内的所有文本节点，并去除首尾空格
        let content_text = element.text().collect::<String>().trim().to_string();

        if content_text.is_empty() {
            continue; // 如果没有文本内容，则跳过
        }

        return content_text
    }

    // 后备方案：DBPedia 页面有时会将主要摘要（通常是英文）放在 <p class="lead"> 标签中
    // 这个标签可能没有 lang 属性。
    // 仅当通过 span 选择器未能提取到英文摘要时，才尝试此方法。
    let selector_lead_paragraph = match Selector::parse(r#"p.lead"#) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "HTML 抓取: URL {} (ID: {}) 解析选择器 'p.lead' 失败: {}.",
                html_url, value_id, e
            );
            // 此处不直接返回，因为可能已经从之前的 span 选择器中找到了中文摘要
            return String::default(); // 或者根据需求决定是否继续（如果已找到中文）
        }
    };
    // 获取第一个匹配的 <p class="lead"> 元素
    if let Some(element) = document.select(&selector_lead_paragraph).next() {
        let content_text = element.text().collect::<String>().trim().to_string();
        if !content_text.is_empty() {
            println!(
                "HTML 抓取: URL {} (ID: {}) 在 'p.lead' 中找到英文摘要。",
                html_url, value_id
            );
            return content_text
        }
    }

    println!(
        "HTML 抓取: URL {} (ID: {}) 未从 HTML 中提取到 'en' 的摘要内容。",
        html_url, value_id
    );

    String::default()
}

// 异步任务函数：抓取网页内容、解析并发送数据
async fn task(
    client: reqwest::Client,           // HTTP 客户端
    tx: Sender<(u64, String)>, // MPSC 发送端
    resource_name: String, // 要抓取的原始 URL (来自 entity2id.json, 例如 "http://dbpedia.org/resource/Example")
    value_id: u64,         // 对应的任务 ID
) {
    // 构建 JSON 数据的 URL，例如 https://dbpedia.org/data/Rust_(programming_language).json
    let json_url = get_json_url(&resource_name);

    let mut attempts = 0; // 当前尝试次数
    let max_attempts = 3; // 最大尝试次数
    let json_response_text: String; // 用于存储 JSON 响应文本

    // 循环尝试获取 JSON 内容，直到成功或达到最大尝试次数
    loop {
        if attempts >= max_attempts {
            eprintln!(
                "JSON 抓取: URL: {} (ID: {}) 在 {} 次尝试后抓取失败。",
                json_url, value_id, max_attempts
            );
            // 同样，发送空 map 表示处理完毕
            if let Err(e) = tx.send((value_id, String::default())).await {
                eprintln!(
                    "URL {} (ID: {}) 在 JSON 抓取达到最大尝试次数后发送空数据失败: {}",
                    json_url, value_id, e
                );
            }
            return;
        }
        attempts += 1;

        // 设置请求超时（例如10秒）
        let response_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.get(&json_url).send(),
        )
        .await;

        match response_result {
            Ok(Ok(response)) => {
                // 请求成功发送并收到响应
                if response.status().is_success() {
                    // HTTP 状态码表示成功
                    match response.text().await {
                        // 尝试获取响应体文本
                        Ok(text) => {
                            json_response_text = text; // 存储成功的响应文本
                            break; // 成功获取文本，跳出循环
                        }
                        Err(e) => {
                            eprintln!(
                                "JSON 抓取尝试 {}: URL {} (ID: {}) 获取响应文本失败: {}",
                                attempts, json_url, value_id, e
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "JSON 抓取尝试 {}: URL {} (ID: {}) 请求失败，状态码: {}",
                        attempts,
                        json_url,
                        value_id,
                        response.status()
                    );
                }
            }
            Ok(Err(e)) => {
                // reqwest 库的错误 (例如网络问题)
                eprintln!(
                    "JSON 抓取尝试 {}: URL {} (ID: {}) 发送请求失败: {}",
                    attempts, json_url, value_id, e
                );
            }
            Err(_) => {
                // Tokio timeout 错误
                eprintln!(
                    "JSON 抓取尝试 {}: URL {} (ID: {}) 请求超时。",
                    attempts, json_url, value_id
                );
            }
        }
        // 发生错误或超时后，等待一段时间再重试（退避策略）
        tokio::time::sleep(std::time::Duration::from_secs(attempts as u64 * 2)).await;
    }

    // 第一次尝试：从 JSON 中提取数据
    // 注意：这里的 key_url 传给 extract_abstracts_from_json 是原始的实体 URL (http://dbpedia.org/resource/...)
    // 因为 DBPedia 的 JSON 数据通常以原始资源 URL 作为顶级键。
    let mut extracted_data =
        extract_abstract_from_json(&json_response_text, &resource_name, value_id).await;

    // 如果 JSON 解析未能提取到摘要，并且 JSON 响应字符串本身并非为空，
    // 则尝试 HTML 抓取作为后备方案。
    if !json_response_text.is_empty() {
        let key_url = format!("http://dbpedia.org/resource/{}", resource_name);
        println!(
            "URL {} (ID: {}) JSON 解析未提取到摘要。启动 HTML 抓取作为后备方案。",
            key_url, value_id
        );
        // `key_url` 是 DBPedia 资源 URL (例如 http://dbpedia.org/resource/...)，适用于 HTML 抓取。
        // `client` 是此任务的 reqwest::Client 实例。
        extracted_data = twice_check(&client, &key_url, value_id).await;

        if extracted_data.is_empty() {
            println!(
                "URL {} (ID: {}) 后备 HTML 抓取也未能提取到 'en' 摘要。",
                key_url, value_id
            );
        } else {
            println!(
                "URL {} (ID: {}) 成功从后备 HTML 抓取中提取到摘要。",
                key_url, value_id
            );
        }
    } else if extracted_data.is_empty() && json_response_text.is_empty() {
        // 这种情况意味着 JSON 抓取成功，但返回了一个空的主体。
        // extract_abstracts_from_json 应该已经通过返回一个空 map 处理了这种情况。
        println!(
            "URL {} (ID: {}) JSON 响应为空，未提取到摘要。跳过 HTML 抓取。",
            json_url, value_id
        );
    }

    // 通过 channel 发送 (ID, 提取到的数据)
    if let Err(e) = tx.send((value_id, extracted_data)).await {
        let key_url = format!("http://dbpedia.org/resource/{}", resource_name);
        eprintln!(
            "URL {} (ID: {}) 发送数据到 channel 失败: {}",
            key_url, value_id, e
        );
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
            Ok(_) => {
                // 处理解析成功但为0的情况
                eprintln!("信号量限制必须是正整数。使用默认值 10。");
                10 // 默认值
            }
            Err(_) => {
                eprintln!(
                    "无法将参数 '{}' 解析为有效的信号量限制 (正整数)。使用默认值 10。",
                    args[1]
                );
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
    let (tx, mut rx) = channel::<(u64, String)>(200);

    // 创建输出文件 item_texts.json
    let file = File::create("item_texts.json")?;
    let mut writer = BufWriter::new(file); // 使用 BufWriter 提高写入效率

    // 派生一个异步任务专门用于写入文件
    let writer_handle = tokio::spawn(async move {
        writer.write_all(b"{")?; // 写入 JSON 对象的起始符 '{'
        let mut first_item = true; // 标记是否是第一个条目，用于处理逗号

        // 循环接收来自其他任务的数据
        while let Some((id, data)) = rx.recv().await {
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
            serde_json::to_writer(&mut writer, &data)?;
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
    for (key_from_json, entity_id) in data.into_iter() {
        // 处理所有条目

        let resource_name = {
            // entity2id.json 中的 DBPedia URL 通常格式为 "<http://dbpedia.org/resource/Example>"
            // 我们需要移除尖括号。
            let url_to_process = if key_from_json.starts_with('<') && key_from_json.ends_with('>') {
                key_from_json[1..key_from_json.len() - 1].to_string()
            } else {
                key_from_json // 如果没有尖括号，则按原样使用
            };

            // 基础 URL 校验，并检查是否为 dbpedia.org 的链接
            if !is_url(&url_to_process) || !url_to_process.contains("dbpedia.org") {
                eprintln!(
                    "URL {} (ID: {}) 无效或不是 dbpedia.org 的 URL。跳过。",
                    url_to_process, entity_id
                );
                // 发送一个空 map，以表示此 ID 已处理（即使没有有效数据）
                // 这样写入器任务就不会永远等待一个从未发送的数据
                if let Err(e) = tx.send((entity_id, String::default())).await {
                    eprintln!(
                        "URL {} (ID: {}) 为无效 URL 发送空数据失败: {}",
                        url_to_process, entity_id, e
                    );
                }
                continue;
            }

            // 从原始 URL 中提取资源名，并构建 DBPedia Data URL (通常以 .json 结尾)
            // 例如: http://dbpedia.org/resource/Rust_(programming_language) -> Rust_(programming_language)
            match url_to_process.rfind('/') {
                Some(idx) => url_to_process[idx + 1..].to_owned(),
                None => {
                    eprintln!(
                        "URL {} (ID: {}) 无法提取资源名。跳过。",
                        url_to_process, entity_id
                    );
                    if let Err(e) = tx.send((entity_id, String::default())).await {
                        eprintln!(
                            "URL {} (ID: {}) 为无资源名 URL 发送空数据失败: {}",
                            url_to_process, e, entity_id
                        );
                    }
                    continue;
                }
            }
        };

        // 为每个任务克隆 tx (Sender)、client 和 semaphore
        let tx_clone = tx.clone();
        let client_clone = client.clone();
        let semaphore_clone = Arc::clone(&semaphore);

        // 派生一个新的异步任务来处理每个 URL
        let handle = tokio::spawn(async move {
            // 在执行任务之前，尝试获取一个信号量许可
            // 这会阻塞直到有许可可用
            let permit = semaphore_clone
                .acquire_owned()
                .await
                .expect("信号量获取失败");

            // 执行实际的任务逻辑
            task(client_clone, tx_clone, resource_name, entity_id).await;

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
    writer_handle
        .await
        .expect("写入任务 join 失败")
        .expect("写入任务执行时 panic");

    println!("处理完成。item_texts.json 文件已创建。");
    Ok(())
}

#[cfg(test)]
mod test {
    use std::io::Read;
    use crate::{extract_abstract_from_json, get_abstract, get_json_url};

    #[tokio::test]
    async fn test_extract_abstracts_from_json() {
        let json_str = reqwest::Client::new()
            .get(get_json_url("Quo_Vadis_(1951_film)"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let r = extract_abstract_from_json(&json_str, "Quo_Vadis_(1951_film)", 5).await;
        println!("{:?}", r);
    }
    
    #[test]
    fn test_get_abstract() {
        let mut file = std::fs::File::open("1979_in_film.json").unwrap();
        let mut str = String::new();
        file.read_to_string(&mut str).unwrap();
        assert_eq!(get_abstract(&*str, "1979_in_film").unwrap(),"The year 1979 in film involved many significant events.")
    }
}

