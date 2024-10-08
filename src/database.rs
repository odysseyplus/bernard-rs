use std::collections::{HashMap, HashSet, VecDeque};
use sqlx::{Sqlite, Transaction};
use crate::fetch::{Change, Item};
use crate::model::{ChangedFile, ChangedFolder, ChangedPath, Drive, File, Folder};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool, SqlitePoolOptions};
use tracing::{debug, error, info, trace, warn};

pub(crate) type Connection = SqliteConnection;

pub(crate) type Pool = SqlitePool;

pub async fn establish_connection(database_path: &str) -> sqlx::Result<Pool> {
    let options = SqliteConnectOptions::default()
        .create_if_missing(true)
        .foreign_keys(true)
        .filename(database_path);

    let pool = SqlitePoolOptions::new().connect_with(options).await?;

    sqlx::migrate!().run(&pool).await?;

    Ok(pool)
}

pub async fn clear_changelog(drive_id: &str, pool: &Pool) -> sqlx::Result<()> {
    ChangedFolder::clear(drive_id, pool).await?;
    ChangedFile::clear(drive_id, pool).await?;

    Ok(())
}

async fn delete_file_or_folder(
    id: &str,
    drive_id: &str,
    conn: &mut Connection,
) -> sqlx::Result<()> {
    Folder::delete(id, drive_id, conn).await?;
    File::delete(id, drive_id, conn).await?;

    Ok(())
}

fn item_to_change(drive_id: &str, item: Item) -> Change {
    match item.drive_id() == drive_id {
        true => Change::ItemChanged(item),
        false => {
            trace!("moved to another shared drive, marked as removed");
            Change::ItemRemoved(item.into_id())
        }
    }
}


fn get_children_ids(folder_id: &str, folders: &Vec<Folder>, files: &Vec<File>) -> (HashSet<String>, HashSet<String>) {
    let mut folder_ids: HashSet<String> = HashSet::new();
    let mut file_ids: HashSet<String> = HashSet::new();

    folder_ids.insert(folder_id.to_string()); // 将初始 folder_id 添加到结果集中

    // 使用循环查找所有子文件夹
    let mut current_level: Vec<&str> = vec![folder_id];
    while !current_level.is_empty() {
        let mut next_level: Vec<&str> = Vec::new();
        for &parent_id in &current_level {
            for folder in folders {
                if folder.parent.as_ref() == Some(&parent_id.to_string()) {
                    folder_ids.insert(folder.id.clone());
                    next_level.push(&folder.id); // 将子文件夹ID添加到下一层级
                }
            }
        }
        current_level = next_level;
    }

    // 查找文件
    for file in files {
        if folder_ids.contains(&file.parent) {
            file_ids.insert(file.id.clone());
        }
    }

    (folder_ids, file_ids)
}

#[tracing::instrument(level = "debug", skip(changes, pool))]
pub async fn merge_changes<I>(
    drive_id: &str,
    changes: I,
    page_token: &str,
    pool: &Pool,
) -> sqlx::Result<()>
where
    I: IntoIterator<Item = Change>,
{
    let mut tx = pool.begin().await?;

    // First update the page_token
    Drive::update_page_token(drive_id, page_token, &mut tx).await?;

    // Query all folder IDs
    let mut folder_ids: HashSet<String> = Folder::get_all_ids(drive_id, &mut tx).await?;
    trace!("Fetched {} folder IDs", folder_ids.len());

    // Query all file IDs
    let mut file_ids: HashSet<String> = File::get_all_ids(drive_id, &mut tx).await?;
    trace!("Fetched {} file IDs", file_ids.len());

    // If an item changes to another drive_id, consider it removed.
    let changes = changes.into_iter().map(|change| match change {
        Change::ItemChanged(item) => item_to_change(drive_id, item),
        _ => change,
    });

    for change in changes {
        match change {
            Change::DriveChanged(drive) => {
                Folder::update_name(drive_id, drive_id, &drive.name, &mut tx).await?
            }
            Change::ItemChanged(item) => match item {
                Item::Folder(folder) => {
                    if folder_ids.contains(&folder.id) {
                        folder.upsert(&mut tx).await?
                    } else {
                        if folder.parent.as_ref().map_or(false, |parent_id| folder_ids.contains(parent_id)) {
                            trace!("New folder ID {} found, adding to database", folder.id);
                            // 执行相应操作
                            folder.create(&mut tx).await?;
                            // 插入成功后，将 ID 添加到 HashSet
                            folder_ids.insert(folder.id.clone());
                        } else {
                            // 如果父文件夹不存在于 folder_ids 中，记录警告信息
                            warn!("Parent folder ID {} not found for new folder ID {}, skipping insertion", folder.parent.as_ref().unwrap(), folder.id);
                        };

                    }
                }
                Item::File(file) => {
                    if file_ids.contains(&file.id) {
                        file.upsert(&mut tx).await?;
                    } else {
                        if folder_ids.contains(&file.parent) {
                            trace!("New file ID {} found, adding to database", file.id);
                            file.create(&mut tx).await?;
                            // 插入成功后，将 ID 添加到 HashSet
                            file_ids.insert(file.id.clone());
                        }else {
                            // 如果父文件夹不存在于 folder_ids 中，记录警告信息
                            warn!("Parent folder ID {} not found for new file ID {}, skipping insertion", file.parent, file.id);
                        };

                    }
                }
            },
            Change::ItemRemoved(id) => {
                let folders: Vec<Folder> = Folder::get_all(drive_id, &mut tx).await?;
                let files: Vec<File> = File::get_all(drive_id, &mut tx).await?;



                if folder_ids.remove(&id) {
                    // 获取所有子文件夹和文件的 ID
                    let (child_folder_ids, child_file_ids) = get_children_ids(&id, &folders, &files);
                    info!(
                        "Need to remove all child_folder_ids {:?} and child_file_ids {:?}",
                        child_folder_ids,
                        child_file_ids
                    );
                    // 从 folder_ids 和 file_ids 中移除所有要删除的 ID
                    for descendant_id in &child_folder_ids {
                        folder_ids.remove(descendant_id);
                    }
                    for descendant_id in &child_file_ids {
                        file_ids.remove(descendant_id);
                    }

                    delete_file_or_folder(&id, drive_id, &mut tx).await?;
                    debug!("Removed folder ID {}", id);
                } else if file_ids.remove(&id) {
                    delete_file_or_folder(&id, drive_id, &mut tx).await?;
                    debug!("Removed file ID {}", id);
                } else {
                    warn!("Item ID {} not found for deletion", id);
                }
            }
            Change::DriveRemoved(_) => (),
        }
    }

    tx.commit().await
}

#[tracing::instrument(level = "debug", skip(name, items, pool))]
pub async fn add_drive(
    drive_id: &str,
    name: &str,
    page_token: &str,
    items: impl IntoIterator<Item = Item>,
    pool: &Pool,
) -> sqlx::Result<()>
{
    let mut tx = pool.begin().await?;

    // Create the drive
    Drive::create(drive_id, page_token, &mut tx).await?;

    // Separate folders and files
    let (folders, files): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| matches!(item, Item::Folder(_)));

    // Create the root folder
    let root_folder = Folder {
        id: drive_id.to_owned(),
        drive_id: drive_id.to_owned(),
        name: name.to_owned(),
        parent: None,
        trashed: false,
    };
    root_folder.create(&mut tx).await?;

    // Create a map of parent_id to vec of child folders
    let mut folder_map: HashMap<Option<String>, Vec<Folder>> = HashMap::new();
    for item in folders {
        if let Item::Folder(folder) = item {
            folder_map.entry(folder.parent.clone()).or_default().push(folder);
        }
    }
    // 打印 folder_map 的值
    debug!("folder_map: {:?}", folder_map);

    // Process folders in level order
    if let Err(e) = process_folders(&mut tx, drive_id, &folder_map, root_folder.id.clone()).await {
        tx.rollback().await?;
        return Err(e);
    }

    // Insert all files
    if let Err(e) = insert_files(&mut tx, &files, &folder_map).await {
        tx.rollback().await?;
        return Err(e);
    }

    // Commit the transaction
    tx.commit().await
}

async fn process_folders(
    tx: &mut Transaction<'_, Sqlite>,
    drive_id: &str,
    folder_map: &HashMap<Option<String>, Vec<Folder>>,
    // 将 root_folder.id 作为参数传递
    root_folder_id: String,
) -> sqlx::Result<()>
{
    let mut queue = VecDeque::new();
    queue.push_back(Some(root_folder_id));
    let mut folder_ids = HashSet::new();
    folder_ids.insert(drive_id.to_string());

    while let Some(parent) = queue.pop_front() {
        if let Some(children) = folder_map.get(&parent) {
            for folder in children {
                if parent.is_some() && !folder_ids.contains(parent.as_ref().unwrap()) {
                    warn!(
                        "Parent folder ID {} not found for new folder ID {}, skipping insertion",
                        parent.as_ref().unwrap(),
                        folder.id
                    );
                    continue;
                }
                match folder.create(tx).await {
                    Ok(_) => {
                        trace!("Created folder: {}", folder.id);
                        folder_ids.insert(folder.id.clone());
                        queue.push_back(Some(folder.id.clone()));
                    }
                    Err(e) => {
                        error!("Failed to create folder {}: {}", folder.id, e);
                    }
                }
            }
        }
    }
    Ok(())
}

async fn insert_files(
    tx: &mut Transaction<'_, Sqlite>,
    files: &[Item],
    folder_map: &HashMap<Option<String>, Vec<Folder>>,
) -> sqlx::Result<()>
{
    for item in files {
        if let Item::File(file) = item {
            if !folder_map.values().flatten().any(|folder| &folder.id == &file.parent) {
                warn!(
                    "Parent folder ID {} not found for new file ID {}, skipping insertion",
                    file.parent,
                    file.id
                );
                continue;
            }
            match file.create(tx).await {
                Ok(_) => {
                    trace!("Created file: {}", file.id);
                }
                Err(e) => {
                    error!("Failed to create file {}: {}", file.id, e);
                }
            }
        }
    }
    Ok(())
}


pub async fn get_drive(drive_id: &str, pool: &Pool) -> sqlx::Result<Option<Drive>> {
    Drive::get_by_id(drive_id, pool).await
}

pub async fn get_changed_files(drive_id: &str, pool: &Pool) -> sqlx::Result<Vec<ChangedFile>> {
    ChangedFile::get_all(drive_id, pool).await
}

pub async fn get_changed_folders(drive_id: &str, pool: &Pool) -> sqlx::Result<Vec<ChangedFolder>> {
    ChangedFolder::get_all(drive_id, pool).await
}

pub async fn get_changed_paths(drive_id: &str, pool: &Pool) -> sqlx::Result<Vec<ChangedPath>> {
    ChangedPath::get_all(drive_id, pool).await
}