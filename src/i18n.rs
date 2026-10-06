//! Interface strings. One table per language, generated from a single list so
//! a string can never exist in one language and be missing in the other.

use crate::config::Lang;

macro_rules! strings {
    ($($field:ident: $en:expr, $ru:expr;)*) => {
        #[derive(Debug)]
        pub struct Strings {
            $(pub $field: &'static str,)*
        }

        const EN: Strings = Strings { $($field: $en,)* };
        const RU: Strings = Strings { $($field: $ru,)* };

        impl Strings {
            /// All `(name, value)` pairs, for tests.
            #[cfg(test)]
            fn entries(&self) -> Vec<(&'static str, &'static str)> {
                vec![$((stringify!($field), self.$field),)*]
            }
        }
    };
}

strings! {
    html_lang: "en", "ru";
    app_title: "Memory review", "Разбор памяти";
    pill_inbox: "inbox", "inbox";
    pill_review: "to review", "к ревью";
    pill_agent: "with agent", "у агента";
    triage_new: "Triage new", "Разобрать новые";
    theme_toggle: "Toggle theme", "Сменить тему";
    filter_open: "Open", "Открытые";
    filter_all: "All", "Все";
    back: "Back", "Назад";
    empty_queue: "Nothing to review. Press “Triage new” when the inbox has notes.", "Разбирать нечего. Нажмите «Разобрать новые», когда в inbox появятся заметки.";
    empty_card: "Select a card on the left.", "Выберите карточку слева.";
    badge_promote: "to verified", "в verified";
    badge_merge: "merge", "слить";
    badge_delete: "delete", "удалить";
    badge_agent: "with agent", "у агента";
    badge_snoozed: "snoozed", "отложено";
    badge_stale: "source changed", "исходник изменился";
    badge_closed: "closed", "закрыто";
    badge_applying: "applying", "применяется";
    badge_accepted: "accepted", "принято";
    tab_diff: "Diff", "Дифф";
    tab_draft: "Draft", "Чистовик";
    tab_sources: "Sources", "Исходники";
    no_draft: "Nothing will be written to verified: the sources are deleted.", "В verified ничего не пишется: исходники удаляются.";
    agent_says: "agent", "агент";
    you_say: "you", "ты";
    system_says: "system", "система";
    draft_version: "draft v", "чистовик v";
    unsent: "not sent yet", "ещё не отправлено";
    comment_placeholder: "Comment for the agent…", "Комментарий для агента…";
    add_comment: "Comment", "Комментировать";
    send_to_agent: "Send to agent", "Отправить агенту";
    accept: "Accept", "Принять";
    accept_again: "Press again to accept", "Нажмите ещё раз";
    snooze: "Snooze", "Отложить";
    regenerate: "Regenerate", "Перегенерировать";
    retry: "Retry", "Повторить";
    agent_working: "The agent is working on this card…", "Агент работает над карточкой…";
    stale_hint: "A source note changed after the draft was made. Regenerate to refresh the proposal.", "Исходная заметка изменилась после подготовки чистовика. Перегенерируйте предложение.";
    closed_hint: "All source notes are gone from the inbox.", "Все исходные заметки исчезли из inbox.";
    applying_hint: "Applying stopped half-way. Retry is safe: finished steps are skipped.", "Применение остановилось на полпути. Повтор безопасен: сделанные шаги пропускаются.";
    accepted_hint: "Accepted and written to memory.", "Принято и записано в память.";
    snoozed_until: "Snoozed until", "Отложено до";
    result_stale: "Not applied: a source changed.", "Не применено: исходник изменился.";
    result_already: "This card was already handled.", "Карточка уже обработана.";
    keys_hint: "J/K move · A accept · S snooze · C comment · ⌘↵ send", "J/K — навигация · A — принять · S — отложить · C — комментарий · ⌘↵ — отправить";
    queued: "queued", "в очереди";
    sources_n: "sources", "источников";
    queue_full: "The agent queue is full; try again in a minute.", "Очередь агента переполнена, повторите через минуту.";
    settings_title: "Settings", "Настройки";
    model_label: "Model", "Модель";
    model_manual: "or type a model name", "или впишите имя модели";
    endpoint_label: "Endpoint (from the environment, read-only)", "Эндпоинт (из окружения, только чтение)";
    models_unavailable: "Could not get the model list from the endpoint; type the name by hand.", "Не удалось получить список моделей с эндпоинта — впишите имя вручную.";
    save: "Save", "Сохранить";
    saved: "Saved. The next agent run uses this model.", "Сохранено. Следующий запуск агента пойдёт через эту модель.";
    bad_model: "Model name must be 1–200 characters without spaces.", "Имя модели — от 1 до 200 символов, без пробелов.";
    what_model_did: "What the model did", "Что сделала модель";
    note_added: "added", "добавлено";
    note_rewritten: "rewritten", "переписано";
    note_removed: "removed", "убрано";
    note_comment: "you", "ты";
    not_found_in_text: "not found in the text", "в тексте не найдено";
    to_version: "on v", "к v";
    comment_fragment: "Comment", "Комментировать";
    comment_on: "Comment on", "Комментарий к";
    cancel: "Cancel", "Отмена";
    draft_changed: "The draft changed since you opened it; look at the new version first.", "Чистовик обновился с тех пор, как вы его открыли, — сначала посмотрите новую версию.";
    show_removed: "show", "показать";
    from_source: "from", "из";
    reprocess: "Reprocess", "Переразобрать";
    reprocess_all: "Reprocess all open cards with this model", "Переразобрать все открытые карточки этой моделью";
    reprocess_all_hint: "Cards to review, snoozed and with changed sources. Threads are kept; each card gets a new draft version.", "Карточки «к ревью», «отложено» и «исходник изменился». Треды сохраняются, у каждой карточки будет новая версия чистовика.";
    press_again: "Press again to confirm", "Нажмите ещё раз для подтверждения";
    requeued: "Sent for reprocessing:", "Отправлено на переразбор:";
}

pub fn strings(lang: Lang) -> &'static Strings {
    match lang {
        Lang::En => &EN,
        Lang::Ru => &RU,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ru_and_en_have_no_empty_strings() {
        for lang in [Lang::En, Lang::Ru] {
            for (name, value) in strings(lang).entries() {
                assert!(!value.trim().is_empty(), "{lang:?}.{name} is empty");
            }
        }
    }

    #[test]
    fn languages_differ_where_expected() {
        assert_eq!(strings(Lang::Ru).accept, "Принять");
        assert_eq!(strings(Lang::En).accept, "Accept");
        assert_eq!(strings(Lang::Ru).html_lang, "ru");
    }
}
