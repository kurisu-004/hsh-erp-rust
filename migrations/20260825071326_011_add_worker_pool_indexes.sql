-- 覆盖 worker_pool::take_one_from_pool CTE 全部谓词：
-- (location, status, current_holder_id, next_process_id)
CREATE INDEX IF NOT EXISTS ix_t_part_batch_pool_pickup
    ON public.t_part_batch
    USING btree (location, status, current_holder_id, next_process_id)
    WHERE deleted_at IS NULL;

-- 覆盖 count_held_by_worker / list_held_by_worker
CREATE INDEX IF NOT EXISTS ix_t_part_batch_holder_location
    ON public.t_part_batch
    USING btree (current_holder_id, location)
    WHERE deleted_at IS NULL;