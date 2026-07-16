use crate::language::{TextKey, text};

// 多处复用的术语集中在这里，避免不同页面使用不一致的翻译。

pub fn user_column_titles() -> [&'static str; 4] {
    // 用户页列标题集中放在这里，避免页面代码里散落多份语言拼装逻辑。
    [
        text(TextKey::User),
        text(TextKey::SessionId),
        text(TextKey::Status),
        text(TextKey::ClientName),
    ]
}

pub fn user_session_column_title() -> &'static str {
    text(TextKey::Session)
}

pub fn network_column_titles() -> [&'static str; 7] {
    // 网络页列标题会被列表头和相关对话框共同复用。
    [
        text(TextKey::Adapter),
        text(TextKey::NetworkUtilization),
        text(TextKey::LinkSpeed),
        text(TextKey::State),
        text(TextKey::BytesSent),
        text(TextKey::BytesReceived),
        text(TextKey::BytesTotal),
    ]
}

pub fn adapter_state(key: &'static str) -> &'static str {
    // 系统状态字符串会先归一成英文键，再映射为当前语言。
    match key {
        "Connected" => text(TextKey::Connected),
        "Disconnected" => text(TextKey::Disconnected),
        "Connecting" => text(TextKey::Connecting),
        "Disconnecting" => text(TextKey::Disconnecting),
        "Hardware Missing" => text(TextKey::HardwareMissing),
        "Hardware Disabled" => text(TextKey::HardwareDisabled),
        "Hardware Malfunction" => text(TextKey::HardwareMalfunction),
        _ => text(TextKey::Unknown),
    }
}

pub fn session_state(key: &'static str) -> &'static str {
    // 会话状态和适配器状态走同一思路：业务层只传稳定键，语言层负责翻译。
    match key {
        "Active" => text(TextKey::Active),
        "Connected" => text(TextKey::Connected),
        "Connect Query" => text(TextKey::ConnectQuery),
        "Shadow" => text(TextKey::Shadow),
        "Disconnected" => text(TextKey::Disconnected),
        "Idle" => text(TextKey::Idle),
        "Listening" => text(TextKey::Listening),
        "Reset" => text(TextKey::Reset),
        "Down" => text(TextKey::Down),
        "Init" => text(TextKey::Init),
        _ => text(TextKey::Unknown),
    }
}
