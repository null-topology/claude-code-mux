use serde_json::Value;

use super::translate::request::{
    ResponsesContentPart, ResponsesFunctionCallOutput, ResponsesFunctionCallOutputContentPart,
    ResponsesInputItem, ResponsesRequest, ResponsesTool,
};

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CodexRequestSizeSummary {
    pub body_json_bytes: u64,
    pub instructions_bytes: u64,
    pub input_json_bytes: u64,
    pub tools_json_bytes: u64,
    pub text_json_bytes: u64,
    pub reasoning_json_bytes: u64,
    pub include_json_bytes: u64,
    pub client_metadata_json_bytes: u64,
    pub input_item_count: usize,
    pub tool_count: usize,
    pub input_image_part_count: usize,
    pub input_image_data_url_bytes: u64,
    pub input_type_counts: std::collections::BTreeMap<String, usize>,
    pub role_counts: std::collections::BTreeMap<String, usize>,
    pub largest_input_items: Vec<InputItemSummary>,
    pub largest_input_images: Vec<InputImageSummary>,
    pub largest_tools: Vec<ToolSummary>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InputItemSummary {
    pub index: usize,
    pub r#type: String,
    pub role: Option<String>,
    pub json_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InputImageSummary {
    pub item_index: usize,
    pub part_index: usize,
    pub json_bytes: u64,
    pub image_url_bytes: u64,
    pub data_url: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolSummary {
    pub index: usize,
    pub name: String,
    pub json_bytes: u64,
}

fn byte_length(s: &str) -> u64 {
    s.len() as u64
}

fn json_bytes(value: Option<&Value>) -> u64 {
    match value {
        Some(v) => byte_length(&serde_json::to_string(v).unwrap_or_default()),
        None => 0,
    }
}

fn input_image_parts(input: &[ResponsesInputItem]) -> Vec<(usize, usize, &str)> {
    let mut parts = Vec::new();
    for (item_idx, item) in input.iter().enumerate() {
        match item {
            ResponsesInputItem::Message { content, .. } => {
                for (part_idx, part) in content.iter().enumerate() {
                    if let ResponsesContentPart::InputImage { image_url, .. } = part {
                        parts.push((item_idx, part_idx, image_url.as_str()));
                    }
                }
            }
            ResponsesInputItem::FunctionCallOutput {
                output: ResponsesFunctionCallOutput::ContentItems(content),
                ..
            } => {
                for (part_idx, part) in content.iter().enumerate() {
                    if let ResponsesFunctionCallOutputContentPart::InputImage {
                        image_url, ..
                    } = part
                    {
                        parts.push((item_idx, part_idx, image_url.as_str()));
                    }
                }
            }
            _ => {}
        }
    }
    parts
}

pub fn summarize_codex_request_size(body: &ResponsesRequest) -> CodexRequestSizeSummary {
    let body_json = serde_json::to_string(body).unwrap_or_default();
    let image_parts = input_image_parts(&body.input);

    let input_type_counts = count_items_by(&body.input, |item| match item {
        ResponsesInputItem::AdditionalTools { .. } => Some("additional_tools".to_string()),
        ResponsesInputItem::Message { .. } => Some("message".to_string()),
        ResponsesInputItem::FunctionCall { .. } => Some("function_call".to_string()),
        ResponsesInputItem::FunctionCallOutput { .. } => Some("function_call_output".to_string()),
        ResponsesInputItem::ToolSearchCall { .. } => Some("tool_search_call".to_string()),
        ResponsesInputItem::ToolSearchOutput { .. } => Some("tool_search_output".to_string()),
        ResponsesInputItem::Reasoning { .. } => Some("reasoning".to_string()),
        ResponsesInputItem::Compaction { .. } => Some("compaction".to_string()),
        ResponsesInputItem::CompactionTrigger => Some("compaction_trigger".to_string()),
    });

    let role_counts = count_items_by(&body.input, |item| match item {
        ResponsesInputItem::AdditionalTools { role, .. } => Some(role.clone()),
        ResponsesInputItem::Message { role, .. } => Some(role.clone()),
        _ => None,
    });

    let largest_input_items = {
        let mut items: Vec<InputItemSummary> = body
            .input
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let (r#type, role) = match item {
                    ResponsesInputItem::AdditionalTools { role, .. } => {
                        ("additional_tools".to_string(), Some(role.clone()))
                    }
                    ResponsesInputItem::Message { role, .. } => {
                        ("message".to_string(), Some(role.clone()))
                    }
                    ResponsesInputItem::FunctionCall { .. } => ("function_call".to_string(), None),
                    ResponsesInputItem::FunctionCallOutput { .. } => {
                        ("function_call_output".to_string(), None)
                    }
                    ResponsesInputItem::ToolSearchCall { .. } => {
                        ("tool_search_call".to_string(), None)
                    }
                    ResponsesInputItem::ToolSearchOutput { .. } => {
                        ("tool_search_output".to_string(), None)
                    }
                    ResponsesInputItem::Reasoning { .. } => ("reasoning".to_string(), None),
                    ResponsesInputItem::Compaction { .. } => ("compaction".to_string(), None),
                    ResponsesInputItem::CompactionTrigger => {
                        ("compaction_trigger".to_string(), None)
                    }
                };
                let json_bytes_val =
                    json_bytes(Some(&serde_json::to_value(item).unwrap_or_default()));
                InputItemSummary {
                    index: i,
                    r#type,
                    role,
                    json_bytes: json_bytes_val,
                }
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.json_bytes));
        items.truncate(5);
        items
    };

    let largest_input_images = {
        let mut items: Vec<InputImageSummary> = image_parts
            .iter()
            .map(|&(item_idx, part_idx, url)| {
                let json_bytes_val = json_bytes(Some(&serde_json::json!({
                    "type": "input_image",
                    "image_url": url,
                })));
                InputImageSummary {
                    item_index: item_idx,
                    part_index: part_idx,
                    json_bytes: json_bytes_val,
                    image_url_bytes: byte_length(url),
                    data_url: url.starts_with("data:"),
                }
            })
            .collect();
        items.sort_by_key(|item| std::cmp::Reverse(item.image_url_bytes));
        items.truncate(5);
        items
    };

    let largest_tools = {
        let mut items: Vec<ToolSummary> = Vec::new();
        if let Some(ref tools) = body.tools {
            for (i, tool) in tools.iter().enumerate() {
                let name = match tool {
                    ResponsesTool::Function(f) => f.name.clone(),
                    ResponsesTool::WebSearch(_) => "web_search".to_string(),
                    ResponsesTool::ToolSearch(_) => "tool_search".to_string(),
                };
                let json_bytes_val =
                    json_bytes(Some(&serde_json::to_value(tool).unwrap_or_default()));
                items.push(ToolSummary {
                    index: i,
                    name,
                    json_bytes: json_bytes_val,
                });
            }
        }
        items.sort_by_key(|item| std::cmp::Reverse(item.json_bytes));
        items.truncate(5);
        items
    };

    CodexRequestSizeSummary {
        body_json_bytes: byte_length(&body_json),
        instructions_bytes: body.instructions.as_ref().map_or(0, |s| byte_length(s)),
        input_json_bytes: json_bytes(Some(&serde_json::to_value(&body.input).unwrap_or_default())),
        tools_json_bytes: match &body.tools {
            Some(tools) => json_bytes(Some(&serde_json::to_value(tools).unwrap_or_default())),
            None => 0,
        },
        text_json_bytes: json_bytes(Some(&serde_json::to_value(&body.text).unwrap_or_default())),
        reasoning_json_bytes: json_bytes(
            body.reasoning
                .as_ref()
                .map(|r| serde_json::to_value(r).unwrap_or_default())
                .as_ref(),
        ),
        include_json_bytes: json_bytes(
            body.include
                .as_ref()
                .map(|i| serde_json::to_value(i).unwrap_or_default())
                .as_ref(),
        ),
        client_metadata_json_bytes: json_bytes(
            body.client_metadata
                .as_ref()
                .map(|m| serde_json::to_value(m).unwrap_or_default())
                .as_ref(),
        ),
        input_item_count: body.input.len(),
        tool_count: body.tools.as_ref().map_or(0, |t| t.len()),
        input_image_part_count: image_parts.len(),
        input_image_data_url_bytes: image_parts
            .iter()
            .filter(|(_, _, url)| url.starts_with("data:"))
            .map(|(_, _, url)| byte_length(url))
            .sum(),
        input_type_counts,
        role_counts,
        largest_input_items,
        largest_input_images,
        largest_tools,
    }
}

fn count_items_by<T, F>(items: &[T], f: F) -> std::collections::BTreeMap<String, usize>
where
    F: Fn(&T) -> Option<String>,
{
    let mut counts = std::collections::BTreeMap::new();
    for item in items {
        if let Some(key) = f(item) {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_simple_request() {
        let input = vec![ResponsesInputItem::Message {
            role: "user".to_string(),
            content: vec![ResponsesContentPart::InputText {
                text: "hello".to_string(),
            }],
        }];
        let req = ResponsesRequest {
            model: "gpt-5.5".to_string(),
            instructions: None,
            input,
            tools: None,
            tool_choice: None,
            store: false,
            stream: true,
            parallel_tool_calls: true,
            include: None,
            client_metadata: None,
            service_tier: None,
            prompt_cache_key: None,
            text: super::super::translate::request::ResponsesText {
                verbosity: Some("low".to_string()),
                format: None,
            },
            reasoning: None,
        };
        let summary = summarize_codex_request_size(&req);
        assert_eq!(summary.input_item_count, 1);
        assert_eq!(summary.tool_count, 0);
        assert!(summary.body_json_bytes > 0);
    }

    #[test]
    fn summarize_with_tools_and_images() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "describe"},
                        {"type": "input_image", "image_url": "data:image/png;base64,abc"}
                    ]
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": [
                        {"type": "input_text", "text": "tool image"},
                        {"type": "input_image", "image_url": "data:image/jpeg;base64,def"}
                    ]
                }
            ],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();
        let summary = summarize_codex_request_size(&req);
        assert_eq!(summary.input_image_part_count, 2);
        assert!(summary.input_image_data_url_bytes > 0);
    }

    fn item_json_bytes(item: &ResponsesInputItem) -> u64 {
        serde_json::to_string(&serde_json::to_value(item).unwrap())
            .unwrap()
            .len() as u64
    }

    #[test]
    fn summarize_reports_tool_search_items_and_tool() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "load a tool"}]
                },
                {
                    "type": "tool_search_call",
                    "call_id": "toolsearch_1",
                    "execution": "client",
                    "status": "completed",
                    "arguments": {"query": "read a file"}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "toolsearch_1",
                    "status": "completed",
                    "execution": "client",
                    "tools": [{
                        "type": "function",
                        "name": "Read",
                        "parameters": {"type": "object"},
                        "defer_loading": true
                    }]
                }
            ],
            "tools": [
                {"type": "function", "name": "Bash", "parameters": {"type": "object"}},
                {
                    "type": "tool_search",
                    "execution": "client",
                    "description": "Search the deferred tools",
                    "parameters": {"type": "object"}
                }
            ],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();

        let summary = summarize_codex_request_size(&req);

        assert_eq!(summary.input_item_count, 3);
        assert_eq!(
            summary.input_type_counts,
            std::collections::BTreeMap::from([
                ("message".to_string(), 1),
                ("tool_search_call".to_string(), 1),
                ("tool_search_output".to_string(), 1),
            ])
        );
        // Tool search items carry no role, so only the message is counted.
        assert_eq!(
            summary.role_counts,
            std::collections::BTreeMap::from([("user".to_string(), 1)])
        );

        let call = summary
            .largest_input_items
            .iter()
            .find(|item| item.r#type == "tool_search_call")
            .expect("tool_search_call is reported");
        assert_eq!(call.index, 1);
        assert_eq!(call.role, None);
        assert_eq!(call.json_bytes, item_json_bytes(&req.input[1]));

        let output = summary
            .largest_input_items
            .iter()
            .find(|item| item.r#type == "tool_search_output")
            .expect("tool_search_output is reported");
        assert_eq!(output.index, 2);
        assert_eq!(output.role, None);
        assert_eq!(output.json_bytes, item_json_bytes(&req.input[2]));

        assert_eq!(summary.tool_count, 2);
        let mut tool_names: Vec<&str> = summary
            .largest_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        tool_names.sort_unstable();
        assert_eq!(tool_names, ["Bash", "tool_search"]);
        assert_eq!(summary.input_image_part_count, 0);
    }

    #[test]
    fn summarize_tolerates_empty_tool_search_shapes() {
        // A search still running (`arguments: null`), one with an empty
        // argument object, an output that loaded nothing, and no tools at all.
        // An entirely absent `arguments` key fails to deserialize, so the
        // summary never sees that shape.
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [
                {
                    "type": "tool_search_call",
                    "call_id": "",
                    "execution": "client",
                    "status": "in_progress",
                    "arguments": null
                },
                {
                    "type": "tool_search_call",
                    "call_id": "toolsearch_1",
                    "execution": "client",
                    "status": "completed",
                    "arguments": {}
                },
                {
                    "type": "tool_search_output",
                    "call_id": "toolsearch_1",
                    "status": "completed",
                    "execution": "client",
                    "tools": []
                }
            ],
            "tools": [],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();

        let summary = summarize_codex_request_size(&req);

        assert_eq!(summary.input_item_count, 3);
        assert_eq!(
            summary.input_type_counts,
            std::collections::BTreeMap::from([
                ("tool_search_call".to_string(), 2),
                ("tool_search_output".to_string(), 1),
            ])
        );
        assert!(summary.role_counts.is_empty());
        assert_eq!(summary.tool_count, 0);
        assert!(summary.largest_tools.is_empty());
        assert_eq!(summary.tools_json_bytes, 2); // "[]"
        assert_eq!(summary.input_image_part_count, 0);
        assert!(summary.body_json_bytes > 0);
    }

    #[test]
    fn summarize_reports_a_tool_search_tool_with_no_items() {
        let req: ResponsesRequest = serde_json::from_value(json!({
            "model": "gpt-5.5",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}]
            }],
            "tools": [{
                "type": "tool_search",
                "execution": "client",
                "description": "Search the deferred tools",
                "parameters": {"type": "object"}
            }],
            "store": false,
            "stream": true,
            "parallel_tool_calls": true,
            "text": {"verbosity": "low"}
        }))
        .unwrap();

        let summary = summarize_codex_request_size(&req);

        assert_eq!(summary.tool_count, 1);
        assert_eq!(summary.largest_tools.len(), 1);
        assert_eq!(summary.largest_tools[0].name, "tool_search");
        assert_eq!(summary.largest_tools[0].index, 0);
        assert!(summary.largest_tools[0].json_bytes > 0);
        assert_eq!(
            summary.input_type_counts,
            std::collections::BTreeMap::from([("message".to_string(), 1)])
        );
    }
}
