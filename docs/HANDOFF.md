# UsageHub: передача наступній сесії

Актуальний знімок — **2026-10-27, v2.22.5**; нижче збережено також історичні результати 2026-10-26. Це не гарантія поточного стану машини. Джерело істини — актуальний код і живі процеси; не завершуйте процес за старим PID з документації. `origin` — власний форк https://github.com/rostikowb/UsageHub, `upstream` — CodeZeno; робоча гілка — `usagehub`, початковий інтеграційний коміт — `9536978`. Не робіть руйнівних `reset` або `clean`.

## v2.22.5: взаємодія, актуальні квоти й власні оновлення

- Встановлено оптимізований `D:/Soft/UsageHub/usagehub.exe`, 8 539 648 байтів, SHA-256 `0E4338639002EB3C8D454472D7E1041E4ACD6A92D312C9839CFA27EFA6D46C43`. Репозиторій — `D:/Work/programming/node/usagehub`; збірка сама не замінює встановлений EXE.
- ПКМ використовує найближчий авторський обробник предків, не губиться на згенерованому дочірньому акаунті. Чисте відкриття меню більше не перемальовує весь бар, вбудоване меню не сканує каталог, sizing використовує кеш геометрії. Нативні вимірювання optimized: 78/33/59 мс; це разові заміри, не гарантія.
- ЛКМ по колонках реально сфокусував вікна Codex і Default Claude за 250 мс. Default активується через `IApplicationActivationManager`; успіх вимагає справжнього атрибутованого вікна. Старий Shell AppsFolder міг повідомити успіх без вікна. Оригінальні іконки вікон отримано через `WM_GETICON`; Default не отримує штучної identity/іконки, чужа `ClaudeAccounts.Second` не змінюється. Додаткові Codex — лише CLI-профілі, не ізольовані Desktop-копії.
- У старому журналі 27% відповідало останньому успішному 73% used, далі Codex багато разів пропускав опитування через busy. Точний власник старого mutex не був записаний, тож не видавайте припущення за встановлену причину. Прибрано `AttachThreadInput` і синхронне відновлення вікна; очікування package activation обмежене 15 с і не тримає lock у COM-потоці. На живій збірці GET Codex знову давав 200 і змінював readings між циклами. Failed/stale summary показує `!`, а не старий відсоток як поточний; невідоме вікно — `--`. Не підставляйте 100% лише з агрегатного `limit_reached`.
- Додавання розташоване над компактними картками: **Налаштування → Акаунти → Claude Code/Codex → назва → Додати акаунт**. Деталі та розширені дії згорнуті. Видалення відкриває окремий scrollable preview в межах viewport; підтвердження stable id і default 7-day backup залишені. На реальній UI 1393×797 перевірено видимість Add, read-only preview Second, прокрутку до disabled Apply і Cancel, після якого акаунт лишився. Apply на реальних акаунтах не натискали, auth не змінювали.
- `src/dashboard.rs` тепер знаходить фактичний заголовок `UsageHub` / winit class з перевіркою EXE; не плутати з прихованим контролером такого самого заголовка. Перевірено, що завершення лише бару закриває його settings-процес, але не Claude/Codex. Це важливо для updater, бо відкритий Studio інакше утримує mapped EXE.
- Portable updater використовує лише [власний stable release](https://github.com/rostikowb/UsageHub/releases/latest), asset `usagehub.exe`, TLS Windows cert store, bounded size і GitHub SHA-256; helper перевіряє staged-файл повторно. WinGet-каналу UsageHub немає. Стару disabled-updater збірку треба один раз замінити вручну. Деталі та межі довіри — `docs/updater.md`. Власний tag workflow налаштовано на MSVC; локальна публікація gnullvm не є доказом запуску hosted MSVC CI.
- Останній повний `cargo test --locked --quiet -- --test-threads=1`: **742 passed, 0 failed, 6 ignored**, 49,40 с виконання тестів. Після нього змінили лише три layout-параметри preview (width/height/scroll), зібрали release і перевірили живу UI. Раніше паралельний запуск мав два timeout loopback HTTP fixtures; це не зелене проходження паралельного набору. Test account mutex namespace тепер відокремлений від реальних акаунтів. Warnings лишилися; `clippy -D warnings` не заявляти зеленим.
- Вимірювання лише resident bar: 42,31 MiB working set, 9,64 MiB private; CPU 0,078 с за 10,07 с. Settings — окремий процес того самого EXE лише на вимогу; ці цифри не включають Studio чи офіційні Claude/Codex.


## Призначення та інваріант квот

UsageHub поєднує керування обліковими записами Claude/Codex і Studio з наявним нативним Rust-баром usageBar. Бар навмисно повторно використано без заміни його оригінального механізму опитування: він уже відображає дані сервера та має працюючу систему тем і нативного вікна. Менеджер не є альтернативним джерелом лімітів.

**Точні квоти і часи reset обов'язково походять з оригінального GET-опитування сервера баром.** Не підставляйте оцінки старого switcher, вручну введені дати, історичну екстраполяцію чи `claude -p`. Якщо сервер не повернув reset, він залишається невідомим. `--manage poll` викликає живий poller і робить фактичний запит; `--manage snapshot` читає лише кеш бару з часовою міткою та ознаками застарілості, не обчислює оцінки. Старий GET-метод polling збережено.

## Де що лежить

- Репозиторій: `D:/Work/programming/node/usagehub`. Встановлений виконуваний файл: `D:/Soft/UsageHub/usagehub.exe`. Ярлик Start Menu **UsageHub** запускає `--dashboard`, щоб під'єднати settings до resident bar та його updater; handoff перевірено без дублювання процесів. `--studio` — standalone editor без monitor owner.
- `src/accounts.rs` — реєстр і властивості профілів; `src/app_settings.rs` — спільні налаштування та кеш. `src/poller/` — наявне серверне опитування, зокрема Claude.
- `src/manager/lifecycle.rs` — життєвий цикл профілів і запуск; `native.rs` — інтеграція з нативним Claude/Windows; `process.rs` — робота з процесами; `backup.rs` — шифровані резервні копії; `cleanup.rs` — preview та контрольоване очищення; `skills.rs` — спільна бібліотека і junction; `credentials.rs` — джерела облікових даних; `paths.rs` — шляхи; `state.rs` — стан і блокування; `cli.rs` — JSON-команди; `migration.rs` — перенесення й rollback; `widget.rs` — інтеграція бару.
- `src/studio_app/studio_manage.rs` — інтерфейс керування Studio. Наявні `src/theme_engine.rs`, `src/theme_engine/account_layout.rs` та `src/window.rs` відповідають за теми, розкладку акаунтів і нативне вікно.
- `docs/migration.md` описує процедури та їхні обмеження; `docs/build-windows.md` — точне середовище збірки. `docs/implementation-contract.md` — історичний початковий ескіз, **не** актуальний контракт Rust API; звіряйте назви й поведінку з кодом.

## Поточні дані та міграція

Робочі дані UsageHub розділені між `%APPDATA%/UsageHub` і `%LOCALAPPDATA%/UsageHub`. Канонічний Desktop Default — External: фізичний каталог `%LOCALAPPDATA%/Claude-Data/claude_pzs8sxrjxfjjc`; окремий CLI home — `%USERPROFILE%/.claude`. Не ототожнюйте два логіни. Second — managed профіль без перенесення, `D:/Soft/claude-multi-account/accounts/Second`; його наявні дані використовуються на місці. Три skill-junction ведуть до `%APPDATA%/UsageHub/skills-library`; на момент міграції бібліотека містила 21 запис.

Обрана тема: `%APPDATA%/UsageHub/themes/left-dock-usage.json`; оригінал `%APPDATA%/ClaudeCodeUsageMonitor/themes/left-dock-usage.json` залишено незміненим. Налаштування, теми, профілі й архіви — дані цієї машини, не файли Git.

Зберігайте справжні каталоги профілів і старі checkout репозиторії, навіть якщо колишній UI виведено з експлуатації. Не створюйте копій plaintext auth bridge. Старі `.claude_backup_*` залишено: для кожного потрібен явний вибір `migration legacy-backup` — `encrypt`, `purge` або `leave`; їх не можна мовчки видаляти.

Резервні копії захищені AES-256-GCM з ключем, захищеним Windows DPAPI CurrentUser, і мають семиденний строк життя. Відновлення прив'язане до користувача/машини DPAPI та наявності не простроченої копії. Очищення без копії вимагає явного `--no-backup`; purge — окрема підтверджувана дія. Звичайне видалення не є гарантією форензичного стирання.

Після cutover заплановане завдання `usageBar Claude accounts sync` — **Disabled**, старий Run `ClaudeCodeUsageMonitor` видалено, старий monitor зупинено. Новий Run запускає лише бар: `"D:/Soft/UsageHub/usagehub.exe" --diagnose` (шлях EXE у лапках). Офіційні Claude та CLI-процеси профілів як і раніше потрібні; це не старі switcher-сервіси, які треба відновлювати.

### Аварійний rollback, не звичайне налаштування

Знімок: `%LOCALAPPDATA%/UsageHub/migration/snapshots/1793048598/rollback.cmd`. Перед запуском зупиніть UsageHub і видаліть або вимкніть **лише його власний** Run, щоб не запустити одночасно старий і новий бар. Абсолютний шлях встановленого EXE має залишатися доступним: скрипт використовує його для відновлення резервних копій. Врахуйте строк життя і прив'язку DPAPI вище. Не запускайте rollback як звичайний setup; перед відновленням прочитайте `docs/migration.md`, зокрема опис можливого часткового стану і відмови при помилці. Видаляючи junction, видаляйте тільки сам inode посилання, ніколи не рекурсивно його ціль чи спільну бібліотеку.

## Збірка Windows

Локальна збірка використовує Rust `1.95.0-x86_64-pc-windows-gnullvm` і LLVM-MinGW під `D:/Tools/usagehub-toolchain`, не глобальну MSVC-інсталяцію. У кожному PowerShell-сеансі:

```powershell
$env:RUSTUP_HOME = 'D:/Tools/usagehub-toolchain/rustup'
$env:CARGO_HOME = 'D:/Tools/usagehub-toolchain/cargo'
$env:PATH = 'D:/Tools/usagehub-toolchain/cargo/bin;D:/Tools/usagehub-toolchain/llvm-mingw/llvm-mingw-20261020-ucrt-x86_64/bin;' + $env:PATH
Set-Location 'D:/Work/programming/node/usagehub'
```

`rust-toolchain.toml` фіксує host, `.cargo/config.toml` — target, linker і staticlibs; глобальний `RUSTUP_TOOLCHAIN` не задавайте. `cargo build` і `cargo build --release` створюють відповідно `target/x86_64-pc-windows-gnullvm/debug/usagehub.exe` та `target/x86_64-pc-windows-gnullvm/release/usagehub.exe`; встановлений EXE — окремий файл, збірка сама його не оновлює. Відомі деталі та залежності — у `docs/build-windows.md`. Усі консольні виклики запускайте зі скінченним deadline; відсутність проміжного виводу не доводить зависання.

### Безпечне захоплення CLI GUI-subsystem

Не передавайте голий GUI EXE в pipeline: повернення керування оболонці не означає завершення процесу. Дочекайтеся процесу та перенаправте обидва потоки, з обмеженим очікуванням. Наприклад, для безпечного `help`:

```powershell
$info = [System.Diagnostics.ProcessStartInfo]::new()
$info.FileName = 'D:/Soft/UsageHub/usagehub.exe'
$info.Arguments = '--manage help'
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.RedirectStandardOutput = $true
$info.RedirectStandardError = $true
$p = [System.Diagnostics.Process]::new()
$p.StartInfo = $info
try {
    if (-not $p.Start()) { throw 'UsageHub CLI failed to start' }
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    if (-not $p.WaitForExit(30000)) {
        $p.Kill()
        if (-not $p.WaitForExit(2000)) {
            throw 'UsageHub CLI did not exit within 2 seconds after termination'
        }
        throw 'UsageHub CLI deadline exceeded'
    }
    if (-not [System.Threading.Tasks.Task]::WaitAll(
        [System.Threading.Tasks.Task[]]@($outTask, $errTask), 2000)) {
        throw 'UsageHub CLI output capture deadline exceeded'
    }
    $stdout = $outTask.Result
    $stderr = $errTask.Result
    "exit=$($p.ExitCode)"
} finally {
    $p.Dispose()
}
```

Оглядайте `$stdout` і `$stderr` лише за потреби, не виводьте токени чи інші секрети в лог/чат. Успішний JSON іде в stdout, помилки — в stderr; коди виходу: 0 успіх, 2 помилка аргументів, 3 операційна помилка. Для read-only діагностики використовуйте `--manage help`, `--manage list`, `--manage status`, `--manage snapshot --provider claude`; snapshot може бути відсутнім або застарілим. `--manage poll --provider claude` — теж read-only щодо стану, але реально звертається до сервера, на відміну від snapshot.

## Ізоляція smoke і чутливі деталі інтеграції

Кожен smoke запускайте в окремих тестових коренях через `USAGEHUB_CONFIG_ROOT`, `USAGEHUB_LOCAL_ROOT`, `USAGEHUB_PROFILES_ROOT`, `USAGEHUB_SHORTCUTS_ROOT` і окремий `TEMP`; не спрямовуйте ці змінні на production. Нативний GUI mutex спільний для production і тестових коренів у межах одного Windows-сеансу (namespace Local): production і тестовий бар одночасно не запускайте. Очищення тестових junction — лише inode, не ціль. Не змінюйте production-профілі заради smoke.

Вже виправлені пастки, які не слід повертати:

- SDK `PROPVARIANT` володіє рядком через `Drop`; додатковий `CoTaskMemFree` призводить до подвійного звільнення.
- Пошук нативного Claude враховує MSIX; використовуйте правильний SDK GUID для `PKEY`.
- Shell parsing paths потребують зворотних слешів і lossless UTF-16, а не втратного перетворення.
- Прив'язка поточного акаунта використовує формат `acct:<account>|<org>:<client>:...` з точним зіставленням UUID, не приблизним пошуком.

## Зафіксовані результати й межі доказів

На 2026-10-26 фінальний набір дав **744 passed, 0 failed, 7 ignored**. Раніше на оптимізованому коді виконано 32 фактичні lifecycle-перевірки; відповідне ядро після цього не змінювали. Завислий child завершився за 1158 мс при таймауті 1 с; для архіву 64 MiB виміряне приватне використання пам'яті — 3,3 MiB. Встановлений EXE мав SHA-256 `2CB463D3A5BE346362778B6D1854E5D975CACB8F815546B612925774703D2248`. Для реальних Default і Second встановлена версія повернула `error: null`; оригінальну тему скопійовано побайтово, SHA-256 `3109A1454243F3597068D11F4F73AD8E4E89E1B15336F2509F708B8F37076982`. Працюючий встановлений бар через 35 с: 39,64 MiB working set, 9,94 MiB private; CPU 0,188 с за 10,06 с спостереження. Це разові результати, не гарантії продуктивності чи поточного стану.

Історичні сім ignored не означали проходження всіх live HTTPS-тестів. Є наявні warnings; не заявляйте, що `clippy -D warnings` зелений. На етапі 2026-10-26 foreground typing не вдалося перевірити через обмеження desktop-автоматизації; backend-створення, welcome офіційного GUI та native preview/backup були перевірені. Тоді публічного релізу ще не було. Актуальний стан взаємодії й каналу оновлень дивіться в секції v2.22.5 вище; локальна збірка не є доказом публікації чи hosted CI.
