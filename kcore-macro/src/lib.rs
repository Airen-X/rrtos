//! kcore-macro：rrtos 过程宏。
//!
//! - `#[task(stack = N, priority = P)]`：把一个 `extern "C" fn ... -> !` 任务函数
//!   扩展为“静态栈 + 静态 TCB + 入口包装 + 同名构造器”，构造器返回
//!   `::kernel::TaskHandle`；
//! - `#[timer(period = N, auto_reload = true/false)]`：把一个定时器回调扩展为
//!   静态 `::kernel::Timer` + 同名构造器（返回 `::kernel::TimerHandle`，未启动）。

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::{
    FnArg, Ident, ItemFn, LitBool, LitInt, Pat, ReturnType, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

// =====================================================================
// #[task] 参数解析
// =====================================================================

struct TaskArgs {
    stack: LitInt,
    priority: LitInt,
}

impl Parse for TaskArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut stack = LitInt::new("512", Span::call_site());
        let mut priority = LitInt::new("1", Span::call_site());

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            if key == "stack" {
                let lit: LitInt = input.parse()?;
                let value: usize = lit.base10_parse()?;
                if value < 128 {
                    return Err(syn::Error::new(lit.span(), "`stack` 不能小于 128 字节"));
                }
                stack = lit;
            } else if key == "priority" {
                let lit: LitInt = input.parse()?;
                let value: u8 = lit.base10_parse()?;
                if value >= 32 {
                    return Err(syn::Error::new(
                        lit.span(),
                        "`priority` 必须在 0..32 之间（实际上限由入口宏的 max_priorities 决定）",
                    ));
                }
                priority = lit;
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "只支持 `stack = <字节>` 与 `priority = <0..7>`",
                ));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(TaskArgs { stack, priority })
    }
}

/// 标记任务，自动建立栈、TCB 与入口包装。
///
/// - 函数必须为 `extern "C"`，返回 `!`，支持 0 或 1 个参数；
/// - `stack` 为任务栈字节数（默认 512，最小 128）；
/// - `priority` 为任务优先级（默认 1，范围 0..7，越大越高）。
///
/// 参数**无需 `Copy`**：构造器会把实参按值“移动”进任务自己的栈，
/// 任务入口再移动取出，因此可传递拥有外设所有权的结构体。
///
/// 生成的同名函数是构造器，返回 `::kernel::TaskHandle`。
#[proc_macro_attribute]
pub fn task(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as TaskArgs);
    let func = parse_macro_input!(item as ItemFn);
    expand_task(&args, &func)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_task(args: &TaskArgs, func: &ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    // ---- 校验 extern "C" ----
    let abi = func
        .sig
        .abi
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(&func.sig, "#[task] 函数必须声明 `extern \"C\"`"))?;
    let abi_name = abi
        .name
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(abi, "缺少 ABI 名称"))?;
    if abi_name.value() != "C" {
        return Err(syn::Error::new_spanned(
            abi_name,
            "#[task] 仅支持 `extern \"C\"`",
        ));
    }

    // ---- 校验返回 `!` ----
    match &func.sig.output {
        ReturnType::Type(_, ty) => {
            if !matches!(**ty, Type::Never(_)) {
                return Err(syn::Error::new_spanned(
                    ty,
                    "#[task] 函数必须返回 `!`（任务不应返回）",
                ));
            }
        }
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                "#[task] 函数必须返回 `!`",
            ));
        }
    }

    // ---- 收集参数（0 或 1 个）----
    let mut arg_idents = Vec::new();
    let mut arg_tys = Vec::new();
    for input in &func.sig.inputs {
        match input {
            FnArg::Typed(pt) => {
                let ty = (*pt.ty).clone();
                let id = match &*pt.pat {
                    Pat::Ident(pi) => pi.ident.clone(),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &pt.pat,
                            "#[task] 仅支持简单标识符参数",
                        ));
                    }
                };
                arg_idents.push(id);
                arg_tys.push(ty);
            }
            FnArg::Receiver(r) => {
                return Err(syn::Error::new_spanned(r, "#[task] 不支持 self 参数"));
            }
        }
    }
    if arg_idents.len() > 1 {
        return Err(syn::Error::new_spanned(
            &func.sig.inputs,
            "#[task] 当前实现仅支持 0 或 1 个参数",
        ));
    }

    let vis = &func.vis;
    let attrs = &func.attrs;
    let fn_name = &func.sig.ident;
    let raw_name = format_ident!("__task_{}__", fn_name);
    let entry_name = format_ident!("__task_entry_{}__", fn_name);
    let stack_ty_name = format_ident!("__TaskStack{}__", fn_name);
    let stack_static = format_ident!("__TASK_STACK_{}__", fn_name);
    let tcb_static = format_ident!("__TASK_TCB_{}__", fn_name);

    let block = &func.block;
    let stack_size = &args.stack;
    let priority = &args.priority;
    let inputs = &func.sig.inputs;

    // ---- 入口包装 body ----
    let entry_body = if arg_idents.is_empty() {
        quote! { #raw_name() }
    } else {
        let id = &arg_idents[0];
        let ty = &arg_tys[0];
        quote! {
            let #id: #ty = unsafe { ::kernel::load_param::<#ty>(__param) };
            #raw_name(#id)
        }
    };

    // ---- 构造器中参数的写入（参数“移动”进任务自己的栈，支持非 Copy 类型）----
    let param_init = if arg_idents.is_empty() {
        quote! { let __param: *mut () = ::core::ptr::null_mut(); }
    } else {
        let id = &arg_idents[0];
        let ty = &arg_tys[0];
        quote! {
            let __frame_size = ::kernel::frame_size();
            // 异常帧起始地址（8 字节对齐，与内核 init_task_stack 一致）
            let __frame_addr =
                ((__stack_base as usize + __stack_size - __frame_size) & !0b111usize) as *mut u8;
            // 参数放在帧下方（低地址侧），按参数类型对齐，避免与异常帧重叠
            let __param_addr = ((__frame_addr as usize - ::core::mem::size_of::<#ty>())
                & !(::core::mem::align_of::<#ty>() - 1)) as *mut u8;
            ::core::debug_assert!((__param_addr as usize) >= (__stack_base as usize));
            let __param_slot = __param_addr as *mut #ty;
            ::core::ptr::write(__param_slot, #id);
            let __param = __param_slot as *mut ();
        }
    };

    let expanded = quote! {
        // 对齐的栈类型
        #[repr(align(8))]
        #[allow(non_camel_case_types)]
        #vis struct #stack_ty_name([::core::mem::MaybeUninit<u8>; #stack_size]);

        #[allow(non_upper_case_globals)]
        static mut #stack_static: #stack_ty_name =
            #stack_ty_name([::core::mem::MaybeUninit::uninit(); #stack_size]);

        #[allow(non_upper_case_globals)]
        static mut #tcb_static: ::kernel::Task = ::kernel::Task::empty();

        // 原函数改名（内部用 Rust ABI：参数可为任意非 FFI-safe 类型，
        // 从而支持拥有外设所有权、不可 Copy 的结构体）
        #(#attrs)*
        #[allow(dead_code)]
        fn #raw_name(#inputs) -> ! {
            #block
        }

        // 包装入口
        unsafe extern "C" fn #entry_name(__param: *mut ()) -> ! {
            #entry_body
        }

        // 同名创建函数：返回任务句柄
        #vis fn #fn_name(#(#arg_idents: #arg_tys),*) -> ::kernel::TaskHandle {
            unsafe {
                let __stack_base = ::core::ptr::addr_of_mut!(#stack_static) as *mut u8;
                let __stack_size = #stack_size;

                #param_init

                ::kernel::task_create(
                    ::core::ptr::addr_of_mut!(#tcb_static),
                    #entry_name,
                    stringify!(#fn_name),
                    __stack_base,
                    __stack_size,
                    __param,
                    #priority,
                )
            }
        }
    };

    Ok(expanded)
}

// =====================================================================
// #[timer] 参数解析与展开
// =====================================================================

struct TimerArgs {
    period: LitInt,
    auto_reload: bool,
}

impl Parse for TimerArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut period = LitInt::new("0", Span::call_site());
        let mut auto_reload = true;
        let mut has_period = false;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            if key == "period" {
                period = input.parse()?;
                has_period = true;
            } else if key == "auto_reload" {
                let b: LitBool = input.parse()?;
                auto_reload = b.value;
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "只支持 `period = <节拍>` 与 `auto_reload = <bool>`",
                ));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        if !has_period {
            return Err(syn::Error::new(
                Span::call_site(),
                "#[timer] 需要 `period = <节拍>`",
            ));
        }

        Ok(TimerArgs { period, auto_reload })
    }
}

/// 标记软件定时器回调。
///
/// - 函数必须为 `extern "C"`，无返回值，参数为 `id: *mut ()`（可省略）；
/// - `period` 为周期节拍数；`auto_reload` 为是否周期重载。
///
/// 生成的同名函数是构造器，返回 `::kernel::TimerHandle`（未启动，需 `timer_start`）。
#[proc_macro_attribute]
pub fn timer(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as TimerArgs);
    let func = parse_macro_input!(item as ItemFn);
    expand_timer(&args, &func)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_timer(args: &TimerArgs, func: &ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    let abi = func.sig.abi.as_ref().ok_or_else(|| {
        syn::Error::new_spanned(&func.sig, "#[timer] 函数必须声明 `extern \"C\"`")
    })?;
    let abi_name = abi
        .name
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(abi, "缺少 ABI 名称"))?;
    if abi_name.value() != "C" {
        return Err(syn::Error::new_spanned(
            abi_name,
            "#[timer] 仅支持 `extern \"C\"`",
        ));
    }

    // 无返回值（`()`）
    if let ReturnType::Type(_, ty) = &func.sig.output {
        if !matches!(**ty, Type::Tuple(ref t) if t.elems.is_empty()) {
            return Err(syn::Error::new_spanned(ty, "#[timer] 回调不能返回值"));
        }
    }

    // 0 或 1 个参数
    let mut arg_id: Option<Ident> = None;
    let mut arg_ty: Option<Type> = None;
    for input in &func.sig.inputs {
        match input {
            FnArg::Typed(pt) => {
                let id = match &*pt.pat {
                    Pat::Ident(pi) => pi.ident.clone(),
                    _ => {
                        return Err(syn::Error::new_spanned(
                            &pt.pat,
                            "#[timer] 仅支持简单标识符参数",
                        ));
                    }
                };
                if arg_id.is_some() {
                    return Err(syn::Error::new_spanned(
                        &pt.pat,
                        "#[timer] 仅支持 0 或 1 个参数",
                    ));
                }
                arg_id = Some(id);
                arg_ty = Some((*pt.ty).clone());
            }
            FnArg::Receiver(r) => {
                return Err(syn::Error::new_spanned(r, "#[timer] 不支持 self 参数"));
            }
        }
    }

    let vis = &func.vis;
    let attrs = &func.attrs;
    let fn_name = &func.sig.ident;
    let raw_name = format_ident!("__timer_raw_{}__", fn_name);
    let cb_name = format_ident!("__timer_cb_{}__", fn_name);
    let timer_static = format_ident!("__TIMER_{}__", fn_name);

    let block = &func.block;
    let inputs = &func.sig.inputs;
    let period = &args.period;
    let auto_reload = args.auto_reload;

    // 回调包装：把 `*mut ()` 转成用户参数类型后调用原函数
    let cb_invoke = if let Some(id) = &arg_id {
        quote! { #raw_name(#id) }
    } else {
        quote! { #raw_name() }
    };
    let cb_setup = if let (Some(id), Some(ty)) = (&arg_id, &arg_ty) {
        quote! { let #id: #ty = __id as #ty; }
    } else {
        quote! {}
    };

    let expanded = quote! {
        #(#attrs)*
        #[allow(dead_code)]
        extern "C" fn #raw_name(#inputs) {
            #block
        }

        #[allow(dead_code)]
        unsafe extern "C" fn #cb_name(__id: *mut ()) {
            #cb_setup
            #cb_invoke
        }

        #[allow(non_upper_case_globals)]
        static mut #timer_static: ::kernel::Timer = ::kernel::Timer::empty();

        #vis fn #fn_name() -> ::kernel::TimerHandle {
            unsafe {
                ::kernel::timer_create(
                    ::core::ptr::addr_of_mut!(#timer_static),
                    #cb_name,
                    stringify!(#fn_name),
                    #period,
                    #auto_reload,
                    ::core::ptr::null_mut(),
                )
            }
        }
    };

    Ok(expanded)
}

// =====================================================================
// #[entry] 入口宏
// =====================================================================

/// 内核入口参数：可选配置项（未提供则用默认值）。
struct EntryArgs {
    tick_rate_hz: Option<LitInt>,
    max_priorities: Option<LitInt>,
    max_tasks: Option<LitInt>,
}

impl Parse for EntryArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = EntryArgs {
            tick_rate_hz: None,
            max_priorities: None,
            max_tasks: None,
        };

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            let lit: LitInt = input.parse()?;

            if key == "tick_rate_hz" {
                args.tick_rate_hz = Some(lit);
            } else if key == "max_priorities" {
                args.max_priorities = Some(lit);
            } else if key == "max_tasks" {
                args.max_tasks = Some(lit);
            } else {
                return Err(syn::Error::new(
                    key.span(),
                    "只支持 `tick_rate_hz` / `max_priorities` / `max_tasks`",
                ));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(args)
    }
}

/// 标记内核入口：完成核心初始化、执行函数体、并启动调度器。
///
/// - 函数无参数，返回 `()` 或 `!`；
/// - 无宏参数时使用默认配置；
/// - 可选参数：`tick_rate_hz`、`max_priorities`、`max_tasks`。
///
/// 示例：
/// ```ignore
/// #[kernel::entry]                       // 默认配置
/// fn main() { /* 创建对象 + kernel::spawn(...) */ }
///
/// #[kernel::entry(tick_rate_hz = 2000, max_priorities = 12)]
/// fn main() { /* ... */ }
/// ```
#[proc_macro_attribute]
pub fn entry(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as EntryArgs);
    let func = parse_macro_input!(item as ItemFn);
    expand_entry(&args, &func)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_entry(args: &EntryArgs, func: &ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    // 校验：无参数
    if !func.sig.inputs.is_empty() {
        return Err(syn::Error::new_spanned(
            &func.sig.inputs,
            "#[entry] 函数不能有参数",
        ));
    }
    // 校验返回类型：() 或 !
    match &func.sig.output {
        ReturnType::Default => {}
        ReturnType::Type(_, ty) => {
            let is_unit = matches!(**ty, Type::Tuple(ref t) if t.elems.is_empty());
            let is_never = matches!(**ty, Type::Never(_));
            if !is_unit && !is_never {
                return Err(syn::Error::new_spanned(
                    ty,
                    "#[entry] 函数必须返回 `()` 或 `!`",
                ));
            }
        }
    }

    let vis = &func.vis;
    let attrs = &func.attrs;
    let fn_name = &func.sig.ident;
    let block = &func.block;

    // 配置初始化语句：有宏参数时覆盖默认值，否则直接用默认配置
    let has_args = args.tick_rate_hz.is_some()
        || args.max_priorities.is_some()
        || args.max_tasks.is_some();
    let config_stmts = if !has_args {
        quote! {
            ::kernel::init(::kernel::KernelConfig::default());
        }
    } else {
        let tick = args
            .tick_rate_hz
            .iter()
            .map(|lit| quote! { __cfg.tick_rate_hz = #lit; });
        let prio = args
            .max_priorities
            .iter()
            .map(|lit| quote! { __cfg.max_priorities = #lit; });
        let tasks = args
            .max_tasks
            .iter()
            .map(|lit| quote! { __cfg.max_tasks = #lit; });
        quote! {
            let mut __cfg = ::kernel::KernelConfig::default();
            #(#tick)*
            #(#prio)*
            #(#tasks)*
            ::kernel::init(__cfg);
        }
    };

    let expanded = quote! {
        #(#attrs)*
        #vis fn #fn_name() -> ! {
            #config_stmts
            #block
            ::kernel::start_scheduler()
        }
    };

    Ok(expanded)
}

// =====================================================================
// 静态内核对象声明宏（惰性初始化）
//
// 每个宏生成：静态控制块（+ 数据缓冲）+ 一个同名访问器函数。
// 访问器在首次调用时在临界区内完成初始化并返回句柄，因此对象无需在
// `#[kernel::entry]` 中手动 `create`。
// =====================================================================

/// `kernel::static_queue!(NAME, Type, LEN)`：静态消息队列。
struct StaticQueueArgs {
    name: Ident,
    ty: Type,
    len: LitInt,
}

impl Parse for StaticQueueArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        input.parse::<Token![,]>()?;
        let ty = input.parse()?;
        input.parse::<Token![,]>()?;
        let len = input.parse()?;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if !input.is_empty() {
            return Err(input.error("参数过多：应为 `名称, 元素类型, 长度`"));
        }
        Ok(Self { name, ty, len })
    }
}

#[proc_macro]
pub fn static_queue(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as StaticQueueArgs);
    expand_static_queue(&args)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_static_queue(args: &StaticQueueArgs) -> syn::Result<proc_macro2::TokenStream> {
    let name = &args.name;
    let ty = &args.ty;
    let len = &args.len;
    let obj = format_ident!("__{}__", name);
    let buf = format_ident!("__{}_BUF__", name);
    let init = format_ident!("__{}_INIT__", name);

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        static mut #obj: ::kernel::Queue = ::kernel::Queue::empty();
        #[allow(non_upper_case_globals)]
        static mut #buf: [u8; #len * ::core::mem::size_of::<#ty>()] =
            [0u8; #len * ::core::mem::size_of::<#ty>()];
        static mut #init: bool = false;

        #[allow(non_snake_case)]
        pub fn #name() -> ::kernel::QueueHandle {
            unsafe {
                ::kernel::enter_critical();
                if !#init {
                    ::kernel::queue_create(
                        ::core::ptr::addr_of_mut!(#obj),
                        ::core::ptr::addr_of_mut!(#buf) as *mut u8,
                        #len,
                        ::core::mem::size_of::<#ty>(),
                        ::kernel::QUEUE_TYPE_BASE,
                    );
                    #init = true;
                }
                ::kernel::exit_critical();
                ::core::ptr::addr_of_mut!(#obj)
            }
        }
    };

    Ok(expanded)
}

/// `kernel::static_mutex!(NAME)`：静态互斥锁。
#[proc_macro]
pub fn static_mutex(input: TokenStream) -> TokenStream {
    let name = parse_macro_input!(input as Ident);
    let obj = format_ident!("__{}__", name);
    let init = format_ident!("__{}_INIT__", name);

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        static mut #obj: ::kernel::Queue = ::kernel::Queue::empty();
        static mut #init: bool = false;

        #[allow(non_snake_case)]
        pub fn #name() -> ::kernel::QueueHandle {
            unsafe {
                ::kernel::enter_critical();
                if !#init {
                    ::kernel::mutex_create(::core::ptr::addr_of_mut!(#obj));
                    #init = true;
                }
                ::kernel::exit_critical();
                ::core::ptr::addr_of_mut!(#obj)
            }
        }
    };

    expanded.into()
}

/// `kernel::static_semaphore!(NAME)`：二值信号量；
/// `kernel::static_semaphore!(NAME, MAX, INITIAL)`：计数信号量。
struct StaticSemaphoreArgs {
    name: Ident,
    counting: Option<(LitInt, LitInt)>,
}

impl Parse for StaticSemaphoreArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        let mut counting = None;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            let max: LitInt = input.parse()?;
            input.parse::<Token![,]>()?;
            let initial: LitInt = input.parse()?;
            counting = Some((max, initial));
        }
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if !input.is_empty() {
            return Err(input.error("参数过多：应为 `名称` 或 `名称, 最大值, 初始值`"));
        }
        Ok(Self { name, counting })
    }
}

#[proc_macro]
pub fn static_semaphore(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as StaticSemaphoreArgs);
    expand_static_semaphore(&args)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

fn expand_static_semaphore(args: &StaticSemaphoreArgs) -> syn::Result<proc_macro2::TokenStream> {
    let name = &args.name;
    let obj = format_ident!("__{}__", name);
    let init = format_ident!("__{}_INIT__", name);

    let create = match &args.counting {
        None => quote! {
            ::kernel::semaphore_create_binary(::core::ptr::addr_of_mut!(#obj));
        },
        Some((max, initial)) => quote! {
            ::kernel::semaphore_create_counting(
                ::core::ptr::addr_of_mut!(#obj),
                #max,
                #initial,
            );
        },
    };

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        static mut #obj: ::kernel::Queue = ::kernel::Queue::empty();
        static mut #init: bool = false;

        #[allow(non_snake_case)]
        pub fn #name() -> ::kernel::QueueHandle {
            unsafe {
                ::kernel::enter_critical();
                if !#init {
                    #create
                    #init = true;
                }
                ::kernel::exit_critical();
                ::core::ptr::addr_of_mut!(#obj)
            }
        }
    };

    Ok(expanded)
}

/// `kernel::static_event_group!(NAME)`：静态事件组。
#[proc_macro]
pub fn static_event_group(input: TokenStream) -> TokenStream {
    let name = parse_macro_input!(input as Ident);
    let obj = format_ident!("__{}__", name);
    let init = format_ident!("__{}_INIT__", name);

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        static mut #obj: ::kernel::EventGroup = ::kernel::EventGroup::empty();
        static mut #init: bool = false;

        #[allow(non_snake_case)]
        pub fn #name() -> ::kernel::EventGroupHandle {
            unsafe {
                ::kernel::enter_critical();
                if !#init {
                    ::kernel::event_group_create(::core::ptr::addr_of_mut!(#obj));
                    #init = true;
                }
                ::kernel::exit_critical();
                ::core::ptr::addr_of_mut!(#obj)
            }
        }
    };

    expanded.into()
}

/// `kernel::static_stream_buffer!(NAME, SIZE, TRIGGER)`：静态流缓冲。
struct StaticStreamBufferArgs {
    name: Ident,
    size: LitInt,
    trigger: Option<LitInt>,
}

impl Parse for StaticStreamBufferArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        input.parse::<Token![,]>()?;
        let size: LitInt = input.parse()?;
        let mut trigger = None;
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            trigger = Some(input.parse()?);
        }
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        }
        if !input.is_empty() {
            return Err(input.error("参数过多：应为 `名称, 字节数[, 触发级别]`"));
        }
        Ok(Self { name, size, trigger })
    }
}

#[proc_macro]
pub fn static_stream_buffer(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as StaticStreamBufferArgs);
    let name = &args.name;
    let size = &args.size;
    let trigger = args.trigger.as_ref().unwrap_or(&args.size);
    let obj = format_ident!("__{}__", name);
    let buf = format_ident!("__{}_BUF__", name);
    let init = format_ident!("__{}_INIT__", name);

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        static mut #obj: ::kernel::StreamBuffer = ::kernel::StreamBuffer::empty();
        #[allow(non_upper_case_globals)]
        static mut #buf: [u8; #size] = [0u8; #size];
        static mut #init: bool = false;

        #[allow(non_snake_case)]
        pub fn #name() -> ::kernel::StreamBufferHandle {
            unsafe {
                ::kernel::enter_critical();
                if !#init {
                    ::kernel::stream_buffer_create(
                        ::core::ptr::addr_of_mut!(#obj),
                        ::core::ptr::addr_of_mut!(#buf) as *mut u8,
                        #size,
                        #trigger,
                    );
                    #init = true;
                }
                ::kernel::exit_critical();
                ::core::ptr::addr_of_mut!(#obj)
            }
        }
    };

    expanded.into()
}

/// `kernel::static_message_buffer!(NAME, SIZE)`：静态消息缓冲。
#[proc_macro]
pub fn static_message_buffer(input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(input as StaticStreamBufferArgs);
    let name = &args.name;
    let size = &args.size;
    let obj = format_ident!("__{}__", name);
    let buf = format_ident!("__{}_BUF__", name);
    let init = format_ident!("__{}_INIT__", name);

    let expanded = quote! {
        #[allow(non_upper_case_globals)]
        static mut #obj: ::kernel::MessageBuffer = ::kernel::MessageBuffer::empty();
        #[allow(non_upper_case_globals)]
        static mut #buf: [u8; #size] = [0u8; #size];
        static mut #init: bool = false;

        #[allow(non_snake_case)]
        pub fn #name() -> ::kernel::MessageBufferHandle {
            unsafe {
                ::kernel::enter_critical();
                if !#init {
                    ::kernel::message_buffer_create(
                        ::core::ptr::addr_of_mut!(#obj),
                        ::core::ptr::addr_of_mut!(#buf) as *mut u8,
                        #size,
                    );
                    #init = true;
                }
                ::kernel::exit_critical();
                ::core::ptr::addr_of_mut!(#obj)
            }
        }
    };

    expanded.into()
}
