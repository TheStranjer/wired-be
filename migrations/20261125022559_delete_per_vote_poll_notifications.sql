-- WBE-807: a ballot no longer notifies the poll's author. Delete the per-vote
-- `poll` rows earlier builds created: the author is the notified user and the
-- voter the actor. The end-of-poll rows stay: the author's own closing has
-- `from_account_id = user_id`, and a voter's closing is notified to someone
-- other than the poll status's author.
DELETE FROM notifications n
      USING statuses s
      WHERE n.type = 'poll'
        AND s.id = n.status_id
        AND n.user_id = s.user_id
        AND n.from_account_id <> n.user_id;
