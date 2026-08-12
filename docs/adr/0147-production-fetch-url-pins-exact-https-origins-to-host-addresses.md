# ADR 0147：Production fetch_url 将Exact HTTPS Origin绑定到Host-pinned Addresses

状态：Accepted
日期：2026-08-13

## 背景

M14已经交付closed/default-off的`ask_user`与三个Workspace filesystem builtins，但`ToolCapabilityClass::Network`仍只有class label，没有任何production resource-level authority consumer。仅把一个任意URL GET executor标成`Network`并关闭redirect/proxy/retry并不构成Sandbox：URL仍可选择任意host，系统DNS可把同一hostname在不同连接时解析到不同地址，redirect可改变authority，环境代理可把流量交给第三方，远端body也可能无限增长或以binary/压缩内容进入模型结果。

现有direct provider adapters已经验证了`reqwest = 0.13.4`在Rust 1.85上的locked-down transport primitives：fixed product User-Agent、no redirect、no automatic retry、no ambient proxy与cancellation-aware bounded streaming。但provider endpoint由host直接安装且请求body/error taxonomy属于Model Gateway；Tools不能依赖OpenAI/Anthropic parser，也不能把“共享HTTP”扩成generic request registry。Network Tool需要自己的真实authority、result contract与Tool lifecycle mapping。

本ADR冻结首个network slice：Runtime-wide、host-installed、exact-origin、address-pinned的HTTPS GET capability，以及一个只返回bounded safe UTF-8 text的closed`fetch_url` builtin。它不建立generic HTTP Tool、arbitrary headers/auth、ambient DNS、HTML parser、download-to-file、web browser、network policy registry、per-Session network mutation或process adapter。

## 决策

1. **Closed builtin with exact surface。** ToolName恰为`fetch_url`，mode为`Parallel`。description恰为：

   > Fetch bounded UTF-8 text with HTTP GET from one host-authorized HTTPS origin and return the response body as a single text part.

   input schema是closed JSON object，恰有一个required string`url`（`maxLength: 4096`），`additionalProperties: false`。semantic constructor要求decoded input是non-empty safe UTF-8 text、不超过4,096 bytes、可解析为absolute URL、无userinfo、无fragment；path与query允许。Tool不接受method、headers、body、credentials、timeout、redirect或response-format参数。

2. **Default-off fifth opt-in and separate authority installation。** `MiniCoreRuntimeConfig::with_fetch_url_tool()`只选择builtin，default-off且idempotent；`MiniCoreRuntimeConfig::with_fetch_url_origin(FetchUrlOrigin)`只安装一个validated authority，单独调用不会披露Tool。`fetch_url` opt-in存在但没有origin、或多个installation归一化到同一canonical origin时，`MiniCoreRuntime::open`以`InvalidConfiguration` fail closed。origin已安装但Tool未opt-in时不materialize client且Runtime ToolSet保持既有selection。selected Tool的client materialization failure映射`RuntimeDependencyUnavailable`，不伪装成配置错误。Production composition冻结五个bool与32种closed selection，definition/spec顺序固定为`ask_user → read_file → list_directory → write_file → fetch_url`；没有generic registry、arbitrary merge、dynamic callback或name-based open-world installation。

3. **Public authority is one redacted exact origin plus pinned addresses。** `FetchUrlOrigin::new(origin, addresses)`是pure validation constructor；其Debug/Display与config Debug不披露origin、hostname、port或addresses，error taxonomy payload-free。`origin`必须是non-empty safe text、最多2,048 bytes、absolute `https` URL，使用DNS hostname而非IPv4/IPv6 literal，userinfo/query/fragment均不存在，path恰为`/`；canonical identity是URL parser归一化后的`https` scheme、DNS hostname与effective port。每个origin提供1..=8个`SocketAddr`，每个port必须等于origin effective port；later duplicate addresses按首次出现顺序移除后仍须非空。unspecified、multicast与IPv4 broadcast地址拒绝；public、private、loopback、link-local与documentation ranges都不由Runtime猜测用途，只在host显式提供exact address时成为authority。

4. **Same-origin is scheme + normalized host + effective port。** Planner同步解析call URL并与已安装canonical origin比较；host大小写、IDNA normalization与explicit/default `:443`按URL parser的canonical/effective语义比较，path与query不参与origin identity。foreign host、subdomain、scheme或port均为Denied。Malformed/oversize URL、userinfo或fragment是invalid arguments。授权返回opaque、redacted、move-only request target，绑定exact parsed URL与exact origin client；executor不重新选择origin、不重新parse string，也不能访问origin集合。

5. **Pinned resolver removes ambient DNS and rebinding。** `open`为每个origin建立一个独立client。client为该exact DNS hostname安装`resolve_to_addrs` static override，并在override下放置一个reject-all resolver；任何未命中hostname都在connect前失败，绝不回退system DNS。URL IP literals已在constructor/planner拒绝，不能绕过override。client只收到其exact-origin authorized target，redirect关闭，因此一次execution只能连接host提供的exact address set。Runtime lifetime内不refresh/re-resolve；address rotation需要host构造新config并重新open Runtime。

6. **TLS/Host identity remains the authorized hostname。** Request URL、HTTP Host与TLS SNI/certificate verification仍使用canonical origin hostname；pinned address只替换socket destination，不把URL改写为IP，不关闭certificate validation，也不安装custom roots或client certificates。Runtime不承诺host提供地址的网络路由、NAT或远端服务所有权，只承诺不向集合外的resolved socket address发起该client的连接。

7. **One per-origin locked-down client。** 每个origin client使用同一protocol-neutral deep transport builder：fixed nonsecret product User-Agent、`redirect::Policy::none()`、`retry::never()`、`no_proxy()`、explicit `no_gzip/no_brotli/no_zstd/no_deflate`与no cookie store。`fetch_url` client在其上再强制`https_only(true)`与`pool_max_idle_per_host(0)`；completed operation不把idle socket保留给later call。这个builder从provider-specific module上提到crate-private shared owner；provider adapters与`fetch_url`是两个真实consumers。Provider error mapping、SSE、JSON envelope与Tool result mapping不共享。per-origin client避免未来HTTP connection reuse把origin A的address authority用于origin B。

8. **Exact request wire。** 每次started execution调用`GET`零或一次，request body为空，只增加固定`Accept: text/plain, application/json`、`Accept-Encoding: identity`与`Connection: close`；不发送Authorization、Cookie、Range、Referer或host-supplied/model-supplied header。3xx不follow，401/403不尝试auth，429/5xx不retry或sleep。connector可在同一GET attempt内尝试最多8个host-pinned addresses建立连接；这不是第二次HTTP request，任何成功connection上至多发送一个GET。`Connection: close`与zero-idle-pool共同保证自然完成后该operation不向client pool转移live connection ownership。

9. **Finite transport lifetime。** client固定`connect_timeout = 10s`与whole request/body `timeout = 30s`。这些不是public knobs，不能被model或per-call修改。ordinary Runtime scheduling不另声明deadline；一旦send future开始poll，network connect、response headers与body stream都受同一30秒request bound。timeout映射为普通Completed Failed，不做retry。

10. **Only successful bounded text is disclosed。** 只有HTTP success status（2xx）继续读取body；其他status不读取或披露error body、status code、headers或Location，直接fixed failure。success response必须恰有一个可解析Content-Type field，其base media type大小写不敏感且恰为`text/plain`或`application/json`；parameters不触发transcoding，bytes仍必须UTF-8。Content-Encoding必须absent，或恰有一个trim后大小写不敏感等于`identity`的field；重复/组合encoding拒绝。不解压、不base64、不解析HTML、不canonicalize JSON。

11. **One safe Text part, exact body bytes。** Content-Length若已知且大于65,536 bytes则在stream前拒绝；否则body以cancellation-aware stream读取至最多65,537 bytes，超过65,536即停止并TooLarge，allocation有界。完整body必须是UTF-8并满足既有safe Text lexical contract，empty允许，不normalize newline、不trim、不重排JSON；success返回exact body text作为一个`ToolResultContent` Text part。远端response URL/body只存在于opaque target/validated success content，不进入Debug、panic、日志或fixed error。

12. **Resource authority and class admission both apply before start。** Valid URL但没有exact installed origin时，planner同步settle`PreExecution + Denied`，start factory与send future都不存在。成功authorization后plan携带exact`ToolCapabilityClass::Network`；ToolSet outer Sandbox必须available exactly forenabled routes所需的class union。class admission不能创建origin authority，origin authority也不能绕过class admission；两者缺一都不得start。

13. **Owner-tracked asynchronous lifecycle without adapter-detached work。** `fetch_url`直接在`ToolOperationSlot`拥有的executor future中poll reqwest send/body futures，不调用raw`tokio::spawn`或blocking worker。start前signal仍由existing gate settlePreExecution Cancelled。start后、send尚未poll时cancellation可证明零request并settleCompleted Cancelled；send/body进行中cancellation会在同一owner future内drop exact send future或response stream并drop其operation-local request state，`Connection: close`与zero-idle-pool禁止把该connection作为later-call resource保留，然后settleCompleted Cancelled。adapter不创建child task或额外cleanup future，也不等待不可信remote acknowledgement；reqwest/hyper自己的transport implementation仍是dependency-internal implementation，不被包装成Runtime-owned task。natural response、timeout、transport error与body validation都在同一个run settlement后进入Terminal。

14. **Fixed model-visible results。** 所有non-success文本fixed、bounded、nonsecret：

   - parse/URL semantic failure：`tool arguments are invalid`，`PreExecution + Failed`；
   - no exact origin authority：`network URL access is denied`，`PreExecution + Denied`；
   - start后cancellation：`URL fetch was cancelled`，`Completed + Cancelled`；
   - connect/send/timeout/body stream/non-2xx failure：`URL could not be fetched`，`Completed + Failed`；
   - missing/unsupported Content-Type或non-identity Content-Encoding：`URL response type is unsupported`，`Completed + Failed`；
   - body超过65,536 bytes：`URL response is too large`，`Completed + Failed`；
   - invalid UTF-8或不满足safe Text contract：`URL response is not valid text`，`Completed + Failed`；
   - success：response body本身，`Completed + Succeeded`；
   - owner invariant/panic使结果不可信：`Abandoned { RuntimeFailure }`，无fabricated text。

15. **Runtime-wide immutable authority, no dynamic revocation claim。** 首版origins属于一个Runtime config，所有使用该production ToolSet的loaded Sessions共享同一immutable origin authority；Workspace reload/invalidation不改变它。没有public wire DTO、per-Agent/per-Session allowlist、credential source、hot reload或unrevoke/revoke method。需要不同network authority的Sessions必须由host放入不同Runtime instance；更细粒度network policy等待真实consumer后另立ADR。

16. **Deterministic loopback evidence stays test-only。** 默认tests不得访问ambient network。production constructor只接受pinned HTTPS DNS origin；module可提供crate-private `#[cfg(test)]` loopback authority/client constructor，允许numeric loopback HTTP且保留exact host/port、no redirect/retry/proxy/compression、body bounds与lifecycle semantics，用于deterministic wire/cancellation tests。该constructor不能被production code、public interface或Runtime config调用，也不能成为HTTP fallback。

## 可执行证据

- authority tests：HTTPS DNS-only origin、canonical default port/host normalization、origin path/query/userinfo/fragment/IP literal rejection、1..=8 addresses、port match、unspecified/multicast/broadcast rejection、duplicate origin fail closed、same-origin path/query authorization与foreign scheme/host/subdomain/port denial；
- resolver tests：authorized hostname只返回pinned addresses，unmatched hostname拒绝且不触发ambient resolver，IP literal不能进入target；
- wire tests：exact GET/path/query/Host/User-Agent/Accept/identity encoding、empty body、no auth/cookie/body、redirect zero-follow、transport failure zero-retry、per-origin clients不交叉addresses；
- response tests：2xx-only、non-2xx body不披露、Content-Type/Content-Encoding matrix、65,536/65,537 boundary、invalid UTF-8、unsafe Text、stream error、fixed timeout；
- lifecycle tests：before-send cancellation零request、mid-headers/mid-body cancellationdrop exact send/response state且无adapter-owned detach或idle-pool handoff、natural result不被late cancellation改写、planner/start/executor panic fail closed；
- composition/Runtime tests：default-off、origin-only undisclosed、tool-without-origin invalid config、idempotent opt-in、32种closed selections/frozen order、Network class admission、local pinned-address end-to-end Turn继续。

## 后果

- `Network`首次同时拥有class-level Sandbox admission与真实resource-level authority；host安装的是socket destination与TLS origin的交集，而不是一枚只能声明不能强制的label。
- static address pinning主动选择availability与authority之间的保守权衡：DNS/CDN地址变化不会被Runtime自动接受，host必须更新config/reopen；这比运行期rebinding或hidden ambient resolver更truthful。
- protocol-neutral locked-down client builder获得第二个真实consumer并上提；provider wire parsers与Tool response policy继续留在各自owner，避免浅层generic HTTP module。
- `fetch_url`只适合host明确授权的bounded text endpoint；arbitrary web browsing、HTML extraction、binary download、authenticated API、redirect chain、custom CA与dynamic DNS policy继续pending。
- process adapter仍不能因为存在`ToolCapabilityClass::Process`就交付；它需要独立的executable/argument authority与跨平台OS sandbox。

## 被否决的方案

- **只allowlist hostname并使用system DNS。** 同一origin可在连接时rebind到未授权address，resource authority不完整。
- **在执行前resolve一次、检查IP范围，然后让reqwest再次resolve。** check与connect不是同一次resolution，仍有TOCTOU/rebinding窗口。
- **只禁止private/loopback地址。** 不能表达host有意授权的internal endpoint，也不能证明public address就是期望resource；exact host-pinned addresses更直接。
- **任意HTTPS URL即安全。** HTTPS提供server identity与transport protection，不提供host authorization、body bounds、redirect或proxy policy。
- **一个client承载全部origins。** 未来connection reuse/coalescing可能使一个origin使用另一个origin的address集合，模糊resource authority。
- **允许redirect但逐跳复验allowlist。** 扩大为multi-request chain、每跳body/cleanup与delivery合同；首版一个GET不需要它。
- **复用provider_transport全部helpers。** Provider delivery/error taxonomy、Retry-After、SSE与JSON envelope不是Tool语义，会制造错误共享owner。
- **支持arbitrary headers/auth。** header值会成为credential与request-smuggling面，需要独立secret source、redaction与policy contract。
- **只依赖Cargo feature关闭compression。** dependency feature unification可能改变行为；builder必须explicit no-compression，response还要拒绝non-identity encoding。
