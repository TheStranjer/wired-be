-- The chat message a `pleroma:chat_mention` notification names, beside
-- `status_id` and `report_id`: a delivered `ChatMessage` is folded into the
-- recipient's chat (`migrations/1791714489_chats.sql`) and its notification is
-- about *that message*, not about a status (`src/federation/handlers/chat.rs`).
-- Pleroma keeps the link as an `activity_id` foreign key onto the chat
-- message's Create activity; decision 6 makes the message a relational row
-- here, so the key points at it and the flat entity renders the message under
-- the `chat_message` key Pleroma's `NotificationView` `put_chat_message` puts
-- for the type. `NULL` for every other type; a hard-deleted message takes its
-- notifications with it, exactly like the status and report arms above.
ALTER TABLE notifications
    ADD COLUMN chat_message_id uuid REFERENCES chat_messages(id) ON DELETE CASCADE;
