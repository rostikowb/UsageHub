# UsageHub: передача наступній сесії

Актуальний знімок — **2026-10-28: 2.22.7 встановлено, перевірено та опубліковано**. Нижче збережено історію попередніх перевірок. Це не гарантія майбутнього стану машини: джерело істини — актуальний код і живі процеси; не завершуйте процес за старим PID. `origin` — https://github.com/rostikowb/UsageHub, `upstream` — CodeZeno; робоча гілка — `usagehub`. Не робіть руйнівних `reset` або `clean`.

## 2.22.7: виправлення іконки й оновлений README

- Після ЛКМ по вже відкритому акаунту Claude відображалася стандартна іконка файлу. Windows `LoadImageW` відхиляє створений UsageHub ICO для Second: 16×16 DIB має height `-16` і лише 32 байти AND mask; іконки офіційного EXE витягуються успішно.
- Фінальне рішення `manager/native.rs`: закешований шлях офіційного EXE використовується у `IShellLink::SetIconLocation(path, 0)`; matching AUMID вікна бере icon/command з цього shortcut. Застарілі relaunch overrides очищуються; лише вже належна цьому акаунту legacy-група перев'язується один раз, стабільний повторний focus не змінює її identity. Controller перевикористовує вже отриманий `ShortcutInfo`, не робить другий ensure. Це відповідає [рекомендації Microsoft](https://learn.microsoft.com/en-us/windows/win32/shell/appids). Numeric-resource варіант відкинуто: у реального Codex лише named `RT_GROUP_ICON` (`IDR_MAINFRAME` тощо), але Shell index 0 успішно витягує оригінальні іконки Claude й Codex. Handmade ICO encoder/creator та дубльований `relaunch_identity` видалено; legacy шлях ICO лишився лише для cleanup/pins. Canonical identity й кольори бару незмінні.
- Успішний `cargo build --release --locked --quiet`, 821.27 с; кінцевий файл має native version **2.22.7.0**, **8 571 904 байти**, SHA-256 **`074624F76DFE155402BAF6B69C41715A6944FDE8F97CD4D8609E6598AFF14F0C`**. Саме його встановлено в `D:/Soft/UsageHub/usagehub.exe` і повторно перевірено hash. Rollback: `D:/Soft/UsageHub/usagehub-2.22.6-before-icons.exe`, hash попереднього release 29A977… .
- Опубліковано [v2.22.7](https://github.com/rostikowb/UsageHub/releases/tag/v2.22.7) з коміту `3c4ba111003830c0b29a6c59dd3abd25e7996aca`. GitHub API підтвердив єдиний `usagehub.exe`, його розмір і `sha256` digest, ідентичний встановленому файлу вище; `/releases/latest` повернув v2.22.7. Старі assets v2.22.5/v2.22.6 не перезаписані. Це локальний gnullvm release, не доказ запуску hosted MSVC CI чи повного різноверсійного updater-download/replace сценарію.
- Native smoke саме нового EXE: перед запуском повернуто legacy icon override з уже виправленим vendor ICO; новий resident прибрав його без закриття Claude. Два реальні ЛКМ по Second — спочатку з іншої програми, потім з уже foreground Claude — зберегли ту саму process birth/PID та AUMID, порожні obsolete relaunch props, правильний Claude logo й видимий бар. Read-only `IShellLink::GetIconLocation` підтвердив офіційний `Claude.exe`, index 0. Screenshot локально: `D:/Temp/usagehub-icon-227-taskbar.png`. Vendor apps і Explorer не перезапускали.
- У 2.22.7 повний suite повторно не запускався: прибрано два incidental ICO tests, виконано release build, перевірку реальним Windows decoder та описаний native consumer smoke. **742 passed / 6 ignored нижче стосуються 2.22.6**, не цього патча; warnings лишилися.
- Resident 2.22.7 запущено Shell.Application поза eval-job; Settings закриті. Показання після перевірки: working set **40.07 MiB**, private **9.62 MiB**, CPU 0.46875 с за 44.188 с (близько 0.088% від усіх logical CPU). Це короткий локальний sample, не гарантія й не пам'ять офіційних Claude/Codex.
- README переписано окремим агентом і перевірено reviewer: **95 рядків**, збережено incoming anchors та `.display` приклад. Commit `ea14e8c` уже на GitHub; 22 локальні links/assets без помилок. Реальний GitHub render показав завантажені badges/preview, без горизонтального overflow; Accounts anchor відкрив потрібну секцію. Verification browser закрито.
- Очищення вже виконано: 5 942 файли, 8 686 724 181 логічний байт; приріст вільного місця D: 7 998 701 568 байтів (18.474 → 26.473 GB). Release cache, дані акаунтів/auth/history/backups, rollback та встановлений EXE збережено. Реальне очищення акаунтів і перезапуск Claude/Codex не дозволені.


## 2.22.6: GPU-налаштування, редактори й фокус бару

### Встановлення та межі публікації

- Історичне встановлення 2.22.6: `D:/Soft/UsageHub/usagehub.exe`, **8 570 368 байтів**, SHA-256 **`29A977EC70D607600E5224840F8F16A15A7E362818EBFF0D3D7BC7176C20644D`**. Сьогоднішній EXE описано в секції 2.22.7 вище; збірка сама його не замінює.
- Backup попереднього встановленого EXE: `D:/Soft/UsageHub/usagehub-2.22.5-before-ui-1793105710.exe`. Це лише виконуваний файл, не резервна копія акаунтів. Для заміни спершу закривайте тільки перевірені процеси UsageHub; Claude/Codex не завершуйте.
- Публічний [release v2.22.6](https://github.com/rostikowb/UsageHub/releases/tag/v2.22.6) опубліковано з коміту `e5b7f81824fcda7fa547e2d70d7ce9f907613fc7`. [Asset `usagehub.exe`](https://github.com/rostikowb/UsageHub/releases/download/v2.22.6/usagehub.exe): 8 570 368 байтів, SHA-256 `29A977EC70D607600E5224840F8F16A15A7E362818EBFF0D3D7BC7176C20644D` — hash історичного встановлення 2.22.6. Опубліковані tag та asset не перезаписуйте; поточне виправлення оформлено окремою 2.22.7.
- Resident запущено через Shell.Application. Native перевірка списку процесів eval-job показала, що resident **не входить у job цієї eval-сесії** й пережив завершення launcher. Сам `IsProcessInJob(..., NULL)` може бути true через інший Windows job; це не доказ прив'язки до агента. Run і Start Menu як раніше вказують на встановлений EXE.
- Активна `left-dock-usage.json` і оригінал старого monitor лишилися byte-identical: SHA-256 `3109A1454243F3597068D11F4F73AD8E4E89E1B15336F2509F708B8F37076982`. Оригінальний дизайн бару не переписано.

### Що й де змінено

- `src/studio_app/warp.rs` — існуючий egui-directx11 painter тепер hardware-first; WARP є **CPU software fallback**, OpenGL/eframe — окремий initialization fallback process. Реально запущено D3D11 hardware на **AMD Radeon (TM) Graphics**, окремо примусово WARP на **Microsoft Basic Render Driver** з успішним кадром і перемиканням вкладки. Не заявляйте живу перевірку OpenGL failover чи Hyper-V: їх тут не запускали.
- `studio_manage.rs`, `studio_settings.rs`, `ui/theme.rs` — компактні таблиці Період/Використано/Скидання, UTC у hover, технічні джерела й додаткові дії в деталях, явна дія очищення Default, читабельні вторинні заголовки. Backup, typed-id, preview binding, explicit no-backup та external-Default opt-in збережено.
- Подальше виправлення `studio_manage.rs` прибрало повтори canonical session/weekly і `weekly all`, але зберігає distinct model/scoped квоти; provenance має власне згорнуте technical disclosure. Замість raw JSON — короткий статус. `studio_core.rs::request_live_usage_refresh` надсилає наявний `WM_APP_REFRESH_NOW` resident monitor; це підтвердження постановки запиту, не готового результату. CLI `--manage poll` навмисно лишається read-only, без запису кешу.
- `studio_theme_workspace.rs`, `studio_context_menus.rs`, `ui/components/layout.rs` — toolbar з перенесенням, окремий компактний Live apply рядок, bounded inspector rows із wrapping labels, горизонтальний scroll workspace. Дочірні panes **звужують успадкований clip**, а не замінюють його. Native smoke виявив і виправив два дефекти, які пропустив source review: full-height `with_layout` робив величезні проміжки, а nested horizontal усередині wrapped toolbar не переносив Live apply атомарно.
- `studio_assets.rs`, `studio_core.rs`, `studio_context_menus.rs` — snapshot ресурсів з інвалідацією на змінах/вході, menu-library discovery лише на потрібній дії, менше повторної файлової роботи.
- `window::query_theme_runtime_for_surface` використовує існуючий geometry snapshot з lazy refresh до разу/секунду в окремому Studio-процесі. `theme_engine/theme_rendering.rs::resolve_scene_preview_bounds` спільно вирішує thumbnail geometry одного surface; прибрано клони scene objects і зайвий clone hit-test theme без overrides. Свіжі usage/language/provider дані не заморожено в кеші whole runtime.
- `manager/native.rs` — first-visible picker більше не вибирає topmost/tool companion замість звичайного, зокрема прихованого primary. Підтримано enabled owned modal chain з bounded traversal, повторну перевірку HWND/PID/birth; без AttachThreadInput, зміни чужих topmost/styles або закриття companion. Якщо primary немає, лишається нормальна official-app activation, а не нова заборона через саму наявність tray helper.
- `window/taskbar_zorder.rs` та wiring — coalesced non-activating restack лише дочірніх поверхонь у shell host, legacy і custom embedding, Explorer hook lifecycle. Це не глобальний topmost і не заміна виправленню неправильного account-window selection.
- Автоматичний `UH_PROFILE_PAGES` видалено. `UH_UI_FRAME_TIMINGS=1` із diagnostics обмежено логує фактичні event-driven кадри; він не перемикає сторінки й не запускає постійну анімацію. Деталі — `dashboard-rendering.md`.

### Вимірювання

CPU часу UI, мс; не загальна input latency і не Present/vsync. Одна машина/тема й hardware D3D11. Baseline — старий WIP з forced page-cycling, після — реальні native кліки й pointer events; перший кадр кожної сторінки виключено. Це локальні samples, не універсальна гарантія.

| Сторінка | Baseline median | Після median | Після p95 |
| --- | ---: | ---: | ---: |
| Settings | 0.660 | 0.598 | 0.848 |
| Theme Studio | 28.953 | 9.207 | 10.266 |
| Context menus | 22.792 | 2.594 | 2.994 |
| Assets | 0.887 | 0.199 | 0.339 |
| Diagnostics | 0.265 | 0.228 | 3.180 |

Холодні UI кадри відрізняються: Settings 85.223 мс, Diagnostics 44.616 мс; не обіцяйте таку саму швидкість першого відкриття, як warmed layout.

| Процес/стан | Working set MiB | Private MiB | CPU секунд / вікно |
| --- | ---: | ---: | ---: |
| Resident bar після взаємодії | 52.56 | 13.88 | 0.15625 / 8 с |
| Settings після обходу редакторів | 186.90 | 171.95 | 0 / 8 с |
| Settings minimized | 186.90 | 171.95 | 0 / 8 с |

Нуль — показання GetProcessTimes у короткому вікні з його квантуванням, не обіцянка нульового CPU. Закриття settings завершує його окремий процес і звільняє його ресурси; minimized зберігає пам'ять.

### Перевірено та свідомо не виконано

- Останній повний `cargo test --locked --quiet -- --test-threads=1` після quota follow-up: **742 passed, 0 failed, 6 ignored**, 74.67 с виконання; release build успішний. Warnings лишилися; `clippy -D warnings` не заявляти зеленим. Три incidental wording/default tests видалено; нові regressions захищають primary/tool/modal selection, distinct model/scoped квоти та cache-consumer transitions.
- Реальні native вкладки й screenshots 1375×750 та 1006×673 client; horizontal scroll не малює поверх sidebar, label/control і Live apply не обрізаються. Перевірено collapse/expand scene root, Tab focus, minimize/restore. Доказові PNG: `D:/Temp/usagehub-ui-final-wide.png`, `D:/Temp/usagehub-ui-final-narrow.png`; це тимчасові локальні артефакти.
- Default cleanup відкрито, Full reset без opt-in лишився заблокованим; закрито без Apply. Реальні акаунти не видаляли/не скидали, login/credentials не вводили, Claude/Codex не зупиняли.
- Реальні ЛКМ Default, Second та **вже запущеного** Codex: окремі правильні Claude PID і звичайне Codex primary, по 40 samples із видимим баром, без повернення foreground до бару. Початковий Codex PID помер за час очікування користувача; під час першої cold-спроби 6-секундний foreground check не пройшов, а нове ordinary вікно з'явилося пізніше. **Повний cold-launch foreground/15-секундний шлях не заявляйте перевіреним**; vendor apps для такого повтору не закривали.
- ПКМ реально відкрив `#32768` popup, закритий Escape; числову latency popup тут не вимірювали. У живому журналі GET Claude/Codex знову давав 200; метод точних квот/reset не змінювали.
- На фінальному встановленому SHA `29A977…` нативно розгорнуто Default details: canonical квоти не повторюються, raw JSON немає. Натискання «Оновити використання зараз» дало `Refresh now received by monitor`, `force=true`, GET 200 для обох Claude і Codex, запис кешу за 581 мс і завантаження цього ж epoch dashboard. Cache updated перейшов `2026-10-27T21:16:30Z` → `21:16:57Z`. Default session reset = null, weekly reset = **2026-11-03T20:00:00Z**, error = null; у фактичному рядку бару **7д видно `100% · 7д`**. PNG: `D:/Temp/usagehub-quota-final-bar.png`. Не приписуйте початкове зникнення дати доведеній помилці parser: на момент перевірки почався новий тижневий період; точно знайдена й виправлена розбіжність — окремий read-only poll проти спільного кешу бару.
- Computer bridge був закритий; використано окремо схвалений користувачем Windows API fallback, із перевіркою exact EXE/PID/birth, foreground/occlusion, відсутності затиснутих кнопок та повторною валідацією перед кліком. При паралельному людському вводі guard перервав дії без кліку; для фінального проходу користувач окремо погодив коротку паузу вводу. Не трактуйте цей дозвіл як дозвіл на майбутні видалення чи нові desktop-сесії.

## v2.22.5: взаємодія, актуальні квоти й власні оновлення

Історичний знімок до поточної 2.22.6; його старий installed hash і перевірки не описують сьогоднішній виконуваний файл.

- Встановлено оптимізований `D:/Soft/UsageHub/usagehub.exe`, 8 539 648 байтів, SHA-256 `0E4338639002EB3C8D454472D7E1041E4ACD6A92D312C9839CFA27EFA6D46C43`. Репозиторій — `D:/Work/programming/node/usagehub`; збірка сама не замінює встановлений EXE.
- ПКМ використовує найближчий авторський обробник предків, не губиться на згенерованому дочірньому акаунті. Чисте відкриття меню більше не перемальовує весь бар, вбудоване меню не сканує каталог, sizing використовує кеш геометрії. Нативні вимірювання optimized: 78/33/59 мс; це разові заміри, не гарантія.
- ЛКМ по колонках реально сфокусував вікна Codex і Default Claude за 250 мс. Default активується через `IApplicationActivationManager`; успіх вимагає справжнього атрибутованого вікна. Старий Shell AppsFolder міг повідомити успіх без вікна. Оригінальні іконки вікон отримано через `WM_GETICON`; Default не отримує штучної identity/іконки, чужа `ClaudeAccounts.Second` не змінюється. Додаткові Codex — лише CLI-профілі, не ізольовані Desktop-копії.
- У старому журналі 27% відповідало останньому успішному 73% used, далі Codex багато разів пропускав опитування через busy. Точний власник старого mutex не був записаний, тож не видавайте припущення за встановлену причину. Прибрано `AttachThreadInput` і синхронне відновлення вікна; очікування package activation обмежене 15 с і не тримає lock у COM-потоці. На живій збірці GET Codex знову давав 200 і змінював readings між циклами. Failed/stale summary показує `!`, а не старий відсоток як поточний; невідоме вікно — `--`. Не підставляйте 100% лише з агрегатного `limit_reached`.
- Додавання розташоване над компактними картками: **Налаштування → Акаунти → Claude Code/Codex → назва → Додати акаунт**. Деталі та розширені дії згорнуті. Видалення відкриває окремий scrollable preview в межах viewport; підтвердження stable id і default 7-day backup залишені. На реальній UI 1393×797 перевірено видимість Add, read-only preview Second, прокрутку до disabled Apply і Cancel, після якого акаунт лишився. Apply на реальних акаунтах не натискали, auth не змінювали.
- `src/dashboard.rs` тепер знаходить фактичний заголовок `UsageHub` / winit class з перевіркою EXE; не плутати з прихованим контролером такого самого заголовка. Перевірено, що завершення лише бару закриває його settings-процес, але не Claude/Codex. Це важливо для updater, бо відкритий Studio інакше утримує mapped EXE.
- Portable updater використовує лише [власний stable release](https://github.com/rostikowb/UsageHub/releases/latest), asset `usagehub.exe`, TLS Windows cert store, bounded size і GitHub SHA-256; helper перевіряє staged-файл повторно. WinGet-каналу UsageHub немає. Стару disabled-updater збірку треба один раз замінити вручну. Деталі та межі довіри — `docs/updater.md`. Власний tag workflow налаштовано на MSVC; локальна публікація gnullvm не є доказом запуску hosted MSVC CI.
- Опубліковано [v2.22.5](https://github.com/rostikowb/UsageHub/releases/tag/v2.22.5) на commit `73087aec2b431b3b2e2020aecf6741775a0045e3`. GitHub API підтвердив єдиний asset `usagehub.exe`, size 8 539 648, digest `sha256:0e4338639002eb3c8d454472d7e1041e4acd6a92d312c9839cfa27efa6d46c43` — той самий, що встановлений. Реальна кнопка оновлень повернула «У вас уже остання версія.»; інформаційний діалог закрито. Це live metadata/check smoke, не перевірка повного різноверсійного завантаження й заміни. Workflow має `overwrite_files: false`, щоб повторний CI не підміняв опублікований локальний EXE.
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

Історичні сім ignored не означали проходження всіх live HTTPS-тестів. Є наявні warnings; не заявляйте, що `clippy -D warnings` зелений. На етапі 2026-10-26 foreground typing не вдалося перевірити через обмеження desktop-автоматизації; backend-створення, welcome офіційного GUI та native preview/backup були перевірені. Тоді публічного релізу ще не було. Актуальні встановлення й native докази дивіться в секціях 2.22.7 та 2.22.6 вище; локальна збірка не є доказом публікації чи hosted CI.
