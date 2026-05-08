//! 运行时对话框模板构建。
//! 项目已经移除了 `.rc`，因此主窗口、页面和辅助对话框都通过这里生成内存模板，
//! 再交给 Win32 的 `CreateDialogIndirectParamW` / `DialogBoxIndirectParamW` 创建。

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM};
use windows_sys::Win32::UI::Controls::{LVS_REPORT, LVS_SINGLESEL};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateDialogIndirectParamW, DialogBoxIndirectParamW, BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON,
    BS_GROUPBOX, BS_OWNERDRAW, ES_AUTOVSCROLL, ES_MULTILINE, SBS_VERT, WS_BORDER, WS_CAPTION,
    WS_CHILD, WS_DISABLED, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_THICKFRAME, WS_VISIBLE,
};

use crate::resource::*;

type DialogProc = Option<unsafe extern "system" fn(HWND, u32, usize, isize) -> isize>;

const DS_SETFONT: u32 = 0x40;
const DS_MODALFRAME: u32 = 0x80;
const DS_3DLOOK: u32 = 0x0004;
const DS_CONTROL: u32 = 0x0400;
const WS_EX_NOPARENTNOTIFY: u32 = 0x0000_0004;
const SS_CENTER_STYLE: u32 = 0x0000_0001;
const BS_OWNERDRAW_STYLE: u32 = BS_OWNERDRAW as u32;
const BS_AUTOCHECKBOX_STYLE: u32 = BS_AUTOCHECKBOX as u32;
const BS_GROUPBOX_STYLE: u32 = BS_GROUPBOX as u32;
const BS_DEFPUSHBUTTON_STYLE: u32 = BS_DEFPUSHBUTTON as u32;
const ES_MULTILINE_STYLE: u32 = ES_MULTILINE as u32;
const ES_AUTOVSCROLL_STYLE: u32 = ES_AUTOVSCROLL as u32;
const SBS_VERT_STYLE: u32 = SBS_VERT as u32;
const DIALOG_FONT_NAME: &str = "MS Shell Dlg";
const DIALOG_FONT_SIZE: u16 = 8;
const CPU_LABELS: [&str; 32] = [
    "CPU 0", "CPU 1", "CPU 2", "CPU 3", "CPU 4", "CPU 5", "CPU 6", "CPU 7", "CPU 8", "CPU 9",
    "CPU 10", "CPU 11", "CPU 12", "CPU 13", "CPU 14", "CPU 15", "CPU 16", "CPU 17", "CPU 18",
    "CPU 19", "CPU 20", "CPU 21", "CPU 22", "CPU 23", "CPU 24", "CPU 25", "CPU 26", "CPU 27",
    "CPU 28", "CPU 29", "CPU 30", "CPU 31",
];

struct ControlSpec<'a> {
    // 单个控件的声明式描述，会被编译进 Win32 `DLGTEMPLATE` 缓冲区。
    class_name: &'a str,
    text: &'a str,
    id: u16,
    style: u32,
    ex_style: u32,
    x: i16,
    y: i16,
    cx: i16,
    cy: i16,
}

struct DialogSpec<'a> {
    // 一整个对话框模板的高层描述。
    style: u32,
    ex_style: u32,
    x: i16,
    y: i16,
    cx: i16,
    cy: i16,
    title: &'a str,
    font_name: &'a str,
    font_size: u16,
    controls: Vec<ControlSpec<'a>>,
}

struct DialogTemplateBuilder {
    // Win32 对话框模板本质上是一段按字对齐的 `u16` 数据流。
    words: Vec<u16>,
}

impl DialogTemplateBuilder {
    fn new() -> Self {
        Self { words: Vec::new() }
    }

    fn build(mut self, spec: DialogSpec<'_>) -> Vec<u16> {
        // 按 `DLGTEMPLATE` / `DLGITEMTEMPLATE` 的内存布局顺序写入模板。
        self.push_u32(spec.style | DS_SETFONT);
        self.push_u32(spec.ex_style);
        self.push_u16(spec.controls.len() as u16);
        self.push_i16(spec.x);
        self.push_i16(spec.y);
        self.push_i16(spec.cx);
        self.push_i16(spec.cy);
        self.push_u16(0);
        self.push_u16(0);
        self.push_str(spec.title);
        self.push_u16(spec.font_size);
        self.push_str(spec.font_name);

        for control in spec.controls {
            self.align_dword();
            self.push_u32(control.style);
            self.push_u32(control.ex_style);
            self.push_i16(control.x);
            self.push_i16(control.y);
            self.push_i16(control.cx);
            self.push_i16(control.cy);
            self.push_u16(control.id);
            self.push_str(control.class_name);
            self.push_str(control.text);
            self.push_u16(0);
        }

        self.words
    }

    fn align_dword(&mut self) {
        // 子控件模板要求按 DWORD 对齐。
        if !self.words.len().is_multiple_of(2) {
            self.words.push(0);
        }
    }

    fn push_u16(&mut self, value: u16) {
        self.words.push(value);
    }

    fn push_i16(&mut self, value: i16) {
        self.words.push(value as u16);
    }

    fn push_u32(&mut self, value: u32) {
        self.words.push((value & 0xFFFF) as u16);
        self.words.push((value >> 16) as u16);
    }

    fn push_str(&mut self, text: &str) {
        self.words.extend(text.encode_utf16());
        self.words.push(0);
    }
}

fn button(
    text: &'static str,
    id: i32,
    style: u32,
    x: i16,
    y: i16,
    cx: i16,
    cy: i16,
) -> ControlSpec<'static> {
    // 标准按钮构造器，供页面模板复用。
    ControlSpec {
        class_name: "Button",
        text,
        id: id as u16,
        style: WS_CHILD | WS_VISIBLE | style,
        ex_style: 0,
        x,
        y,
        cx,
        cy,
    }
}

fn static_text(
    text: &'static str,
    id: i32,
    style: u32,
    x: i16,
    y: i16,
    cx: i16,
    cy: i16,
) -> ControlSpec<'static> {
    // 静态文本构造器。
    ControlSpec {
        class_name: "Static",
        text,
        id: id as u16,
        style: WS_CHILD | WS_VISIBLE | style,
        ex_style: 0,
        x,
        y,
        cx,
        cy,
    }
}

fn edit_text(id: i32, style: u32, x: i16, y: i16, cx: i16, cy: i16) -> ControlSpec<'static> {
    // 编辑框构造器。
    ControlSpec {
        class_name: "Edit",
        text: "",
        id: id as u16,
        style: WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | style,
        ex_style: 0,
        x,
        y,
        cx,
        cy,
    }
}

fn ownerdraw_button(
    id: i32,
    visible: bool,
    x: i16,
    y: i16,
    cx: i16,
    cy: i16,
) -> ControlSpec<'static> {
    // 仪表和图表控件仍然借用 owner-draw button 的 Win32 事件模型。
    ControlSpec {
        class_name: "Button",
        text: "OD",
        id: id as u16,
        style: WS_CHILD | if visible { WS_VISIBLE } else { 0 } | WS_DISABLED | BS_OWNERDRAW_STYLE,
        ex_style: 0,
        x,
        y,
        cx,
        cy,
    }
}

fn frame_control(
    text: &'static str,
    id: i32,
    x: i16,
    y: i16,
    cx: i16,
    cy: i16,
) -> ControlSpec<'static> {
    // 自定义 frame 类名用于兼容经典任务管理器那种分组框视觉。
    ControlSpec {
        class_name: "TaskManagerFrame",
        text,
        id: id as u16,
        style: WS_CHILD | WS_VISIBLE | BS_GROUPBOX_STYLE,
        ex_style: WS_EX_NOPARENTNOTIFY,
        x,
        y,
        cx,
        cy,
    }
}

fn build_main_dialog() -> DialogSpec<'static> {
    // 主窗口目前只承载标签页控件，其余页面由子对话框填充。
    DialogSpec {
        style: DS_3DLOOK | WS_POPUP | WS_CAPTION | WS_THICKFRAME,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 264,
        cy: 247,
        title: "",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls: vec![ControlSpec {
            class_name: "SysTabControl32",
            text: "Tab1",
            id: IDC_TABS as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            ex_style: 0,
            x: 3,
            y: 3,
            cx: 91,
            cy: 116,
        }],
    }
}

fn build_perf_dialog() -> DialogSpec<'static> {
    // 性能页模板保留了原始控件编号和大致布局比例，方便兼容旧逻辑。
    let mut controls = vec![
        ownerdraw_button(IDC_CPUMETER, true, 14, 17, 48, 44),
        ownerdraw_button(IDC_MEMMETER, true, 14, 77, 48, 44),
        static_text("Handles", IDC_STATIC14, 0, 14, 139, 32, 8),
        static_text("Threads", IDC_STATIC15, 0, 14, 148, 32, 8),
        static_text("Processes", IDC_STATIC16, 0, 14, 157, 34, 8),
        static_text("0", IDC_TOTAL_HANDLES, 0x0002, 59, 139, 57, 8),
        static_text("0", IDC_TOTAL_THREADS, 0x0002, 59, 148, 57, 8),
        static_text("0", IDC_TOTAL_PROCESSES, 0x0002, 59, 157, 57, 8),
        static_text("Total", IDC_STATIC2, 0, 136, 139, 32, 8),
        static_text("Available", IDC_STATIC3, 0, 136, 148, 32, 8),
        static_text("File Cache", IDC_STATIC4, 0, 136, 157, 34, 8),
        static_text("0", IDC_TOTAL_PHYSICAL, 0x0002, 182, 139, 57, 8),
        static_text("0", IDC_AVAIL_PHYSICAL, 0x0002, 182, 148, 57, 8),
        static_text("0", IDC_FILE_CACHE, 0x0002, 182, 157, 57, 8),
        static_text("Total", IDC_STATIC6, 0, 14, 183, 32, 8),
        static_text("Limit", IDC_STATIC8, 0, 14, 192, 32, 8),
        static_text("Peak", IDC_STATIC9, 0, 14, 201, 32, 8),
        static_text("0", IDC_COMMIT_TOTAL, 0x0002, 59, 183, 57, 8),
        static_text("0", IDC_COMMIT_LIMIT, 0x0002, 59, 192, 57, 8),
        static_text("0", IDC_COMMIT_PEAK, 0x0002, 59, 201, 57, 8),
        static_text("Total", IDC_STATIC11, 0, 136, 183, 32, 8),
        static_text("Paged", IDC_STATIC12, 0, 136, 192, 32, 8),
        static_text("Nonpaged", IDC_STATIC17, 0, 136, 201, 34, 8),
        static_text("0", IDC_KERNEL_TOTAL, 0x0002, 182, 183, 57, 8),
        static_text("0", IDC_KERNEL_PAGED, 0x0002, 182, 192, 57, 8),
        static_text("0", IDC_KERNEL_NONPAGED, 0x0002, 182, 201, 57, 8),
        frame_control("CPU Usage History", IDC_CPUFRAME, 78, 5, 8, 60),
        ownerdraw_button(IDC_MEMGRAPH, true, 82, 77, 48, 44),
        frame_control("CPU Usage", IDC_CPUUSAGEFRAME, 8, 5, 60, 60),
        frame_control("MEM Usage", IDC_MEMBARFRAME, 8, 67, 60, 60),
        frame_control("Memory Usage History", IDC_MEMFRAME, 78, 67, 60, 60),
        frame_control("Physical Memory (K)", IDC_STATIC1, 130, 129, 114, 39),
        frame_control("Commit Charge (K)", IDC_STATIC5, 8, 173, 113, 39),
        frame_control("Kernel Memory (K)", IDC_STATIC10, 130, 173, 114, 39),
        frame_control("Totals", IDC_STATIC13, 8, 129, 113, 39),
    ];

    for index in 0..STATIC_CPU_GRAPH_COUNT {
        controls.push(ownerdraw_button(
            IDC_CPUGRAPH + index as i32,
            index == 0,
            82 + (index as i16 * 7),
            15,
            13,
            44,
        ));
    }

    DialogSpec {
        style: DS_CONTROL | WS_CHILD,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 438,
        cy: 303,
        title: "",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls,
    }
}

fn build_network_dialog() -> DialogSpec<'static> {
    DialogSpec {
        style: DS_CONTROL | WS_CHILD,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 438,
        cy: 303,
        title: "",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls: vec![
            ControlSpec {
                class_name: "SysListView32",
                text: "List1",
                id: IDC_NICTOTALS as u16,
                style: WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL,
                ex_style: 0,
                x: 9,
                y: 9,
                cx: 376,
                cy: 131,
            },
            static_text(
                "No Active Network Adapters Found.",
                IDC_NOADAPTERS,
                SS_CENTER_STYLE,
                66,
                144,
                262,
                8,
            ),
            ControlSpec {
                class_name: "ScrollBar",
                text: "",
                id: IDC_GRAPHSCROLLVERT as u16,
                style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | SBS_VERT_STYLE,
                ex_style: 0,
                x: 390,
                y: 9,
                cx: 10,
                cy: 131,
            },
        ],
    }
}

fn build_process_dialog() -> DialogSpec<'static> {
    DialogSpec {
        style: DS_3DLOOK | DS_CONTROL | WS_CHILD,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 393,
        cy: 197,
        title: "",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls: vec![
            ControlSpec {
                class_name: "SysListView32",
                text: "List2",
                id: IDC_PROCLIST as u16,
                style: WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL,
                ex_style: 0,
                x: 9,
                y: 9,
                cx: 376,
                cy: 131,
            },
            button("&End Process", IDC_TERMINATE, 0, 320, 144, 66, 14),
        ],
    }
}

fn build_task_dialog() -> DialogSpec<'static> {
    DialogSpec {
        style: DS_3DLOOK | DS_CONTROL | WS_CHILD,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 393,
        cy: 177,
        title: "",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls: vec![
            ControlSpec {
                class_name: "SysListView32",
                text: "List2",
                id: IDC_TASKLIST as u16,
                style: WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | LVS_REPORT,
                ex_style: 0,
                x: 9,
                y: 9,
                cx: 378,
                cy: 139,
            },
            button(
                "&Switch To",
                IDC_SWITCHTO,
                BS_DEFPUSHBUTTON_STYLE,
                280,
                152,
                53,
                14,
            ),
            button("&End Task", IDC_ENDTASK, 0, 224, 152, 53, 14),
            button(
                "&New Task...",
                IDM_RUN as i32,
                BS_DEFPUSHBUTTON_STYLE,
                336,
                152,
                53,
                14,
            ),
        ],
    }
}

fn build_select_columns_dialog() -> DialogSpec<'static> {
    let controls = vec![
        button("OK", 1, BS_DEFPUSHBUTTON_STYLE, 83, 122, 50, 14),
        button("Cancel", 2, 0, 137, 122, 50, 14),
        static_text(
            "Select the columns that will appear on the Process page of the Task Manager.",
            IDC_SELECTPROCCOLS_DESC,
            0,
            7,
            4,
            177,
            19,
        ),
        ControlSpec {
            class_name: "Button",
            text: "&Image Name",
            id: IDC_IMAGENAME as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | WS_DISABLED | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 26,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "PID (Process Identifier)",
            id: IDC_PID as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 37,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "User Name",
            id: IDC_USERNAME as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 48,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Session ID",
            id: IDC_SESSIONID as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 59,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "CPU Usage",
            id: IDC_CPU as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 70,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "CPU Time",
            id: IDC_CPUTIME as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 81,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Memory Usage",
            id: IDC_MEMUSAGE as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 92,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Memory Usage Delta",
            id: IDC_MEMUSAGEDIFF as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 7,
            y: 103,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Page Faults",
            id: IDC_PAGEFAULTS as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 26,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Page Faults Delta",
            id: IDC_PAGEFAULTSDIFF as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 37,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Virtual Memory Size",
            id: IDC_COMMITCHARGE as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 48,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Paged Pool",
            id: IDC_PAGEDPOOL as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 59,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Non-paged Pool",
            id: IDC_NONPAGEDPOOL as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 70,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Base Priority",
            id: IDC_BASEPRIORITY as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 81,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Handle Count",
            id: IDC_HANDLECOUNT as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 92,
            cx: 87,
            cy: 10,
        },
        ControlSpec {
            class_name: "Button",
            text: "Thread Count",
            id: IDC_THREADCOUNT as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x: 103,
            y: 103,
            cx: 87,
            cy: 10,
        },
    ];
    DialogSpec {
        style: DS_MODALFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU,
        ex_style: 0,
        x: 20,
        y: 20,
        cx: 191,
        cy: 141,
        title: "Select Columns",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls,
    }
}

fn build_affinity_dialog() -> DialogSpec<'static> {
    let mut controls = Vec::new();
    for cpu_index in 0..=31 {
        let (column, row) = (cpu_index / 8, cpu_index % 8);
        let x = match column {
            0 => 13,
            1 => 65,
            2 => 119,
            _ => 178,
        };
        let cx = if cpu_index >= 10 { 41 } else { 37 };
        controls.push(ControlSpec {
            class_name: "Button",
            text: CPU_LABELS[cpu_index as usize],
            id: (IDC_CPU0 + cpu_index) as u16,
            style: WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_AUTOCHECKBOX_STYLE,
            ex_style: 0,
            x,
            y: 35 + (row as i16 * 12),
            cx,
            cy: 10,
        });
    }
    controls.push(ControlSpec {
        class_name: "Button",
        text: "Static",
        id: IDC_AFFINITY_GROUP as u16,
        style: WS_CHILD | WS_VISIBLE | BS_GROUPBOX_STYLE,
        ex_style: 0,
        x: 7,
        y: 25,
        cx: 217,
        cy: 108,
    });
    controls.push(button("OK", 1, BS_DEFPUSHBUTTON_STYLE, 121, 138, 50, 14));
    controls.push(button("Cancel", 2, 0, 175, 138, 50, 14));
    controls.push(static_text(
        "The Processor Affinity setting controls which CPUs the process will be allowed to execute on.",
        IDC_AFFINITY_DESC,
        0,
        7,
        6,
        218,
        19,
    ));
    DialogSpec {
        style: DS_MODALFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU,
        ex_style: 0,
        x: 20,
        y: 20,
        cx: 232,
        cy: 157,
        title: "Processor Affinity",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls,
    }
}

fn build_users_dialog() -> DialogSpec<'static> {
    DialogSpec {
        style: DS_3DLOOK | DS_CONTROL | WS_CHILD,
        ex_style: 0,
        x: 0,
        y: 0,
        cx: 393,
        cy: 197,
        title: "",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls: vec![
            ControlSpec {
                class_name: "SysListView32",
                text: "List3",
                id: IDC_USERLIST as u16,
                style: WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP | LVS_REPORT,
                ex_style: 0,
                x: 9,
                y: 9,
                cx: 376,
                cy: 131,
            },
            button("&Disconnect", IDM_DISCONNECT as i32, 0, 203, 144, 55, 14),
            button("&Logoff", IDM_LOGOFF as i32, 0, 263, 144, 52, 14),
            button(
                "&Send Message...",
                IDM_SENDMESSAGE as i32,
                BS_DEFPUSHBUTTON_STYLE,
                320,
                144,
                66,
                14,
            ),
        ],
    }
}

fn build_message_dialog() -> DialogSpec<'static> {
    DialogSpec {
        style: DS_MODALFRAME | WS_POPUP | WS_CAPTION | WS_SYSMENU,
        ex_style: 0,
        x: 20,
        y: 20,
        cx: 214,
        cy: 114,
        title: "Send Message",
        font_name: DIALOG_FONT_NAME,
        font_size: DIALOG_FONT_SIZE,
        controls: vec![
            static_text("&Message title:", IDC_MESSAGE_TITLE_LABEL, 0, 7, 7, 200, 8),
            edit_text(
                IDC_MESSAGE_TITLE,
                ES_MULTILINE_STYLE | ES_AUTOVSCROLL_STYLE,
                7,
                17,
                200,
                25,
            ),
            static_text("Me&ssage:", IDC_MESSAGE_BODY_LABEL, 0, 7, 50, 200, 8),
            edit_text(
                IDC_MESSAGE_MESSAGE,
                ES_MULTILINE_STYLE | ES_AUTOVSCROLL_STYLE,
                7,
                60,
                200,
                25,
            ),
            button("OK", 1, BS_DEFPUSHBUTTON_STYLE, 49, 95, 50, 14),
            button("Cancel", 2, 0, 103, 95, 50, 14),
        ],
    }
}

fn dialog_spec(dialog_id: u16) -> Option<DialogSpec<'static>> {
    Some(match dialog_id {
        IDD_MAINWND => build_main_dialog(),
        IDD_PERFPAGE => build_perf_dialog(),
        IDD_NETPAGE => build_network_dialog(),
        IDD_PROCPAGE => build_process_dialog(),
        IDD_TASKPAGE => build_task_dialog(),
        IDD_SELECTPROCCOLS => build_select_columns_dialog(),
        IDD_AFFINITY => build_affinity_dialog(),
        IDD_USERSPAGE => build_users_dialog(),
        IDD_MESSAGE => build_message_dialog(),
        _ => return None,
    })
}

pub fn create_dialog(
    hinstance: HINSTANCE,
    dialog_id: u16,
    parent: HWND,
    dialog_proc: DialogProc,
    init_param: LPARAM,
) -> HWND {
    let Some(spec) = dialog_spec(dialog_id) else {
        return std::ptr::null_mut();
    };
    let template = DialogTemplateBuilder::new().build(spec);
    // SAFETY: the generated template buffer is valid for the duration of the call; Win32 copies
    // or consumes it before returning the dialog handle.
    unsafe {
        CreateDialogIndirectParamW(
            hinstance,
            template.as_ptr() as *const _,
            parent,
            dialog_proc,
            init_param,
        )
    }
}

pub fn dialog_box(
    hinstance: HINSTANCE,
    dialog_id: u16,
    parent: HWND,
    dialog_proc: DialogProc,
    init_param: LPARAM,
) -> isize {
    let Some(spec) = dialog_spec(dialog_id) else {
        return -1;
    };
    let template = DialogTemplateBuilder::new().build(spec);
    // SAFETY: the generated template buffer remains alive while the modal dialog is created and
    // run by `DialogBoxIndirectParamW`.
    unsafe {
        DialogBoxIndirectParamW(
            hinstance,
            template.as_ptr() as *const _,
            parent,
            dialog_proc,
            init_param,
        )
    }
}
