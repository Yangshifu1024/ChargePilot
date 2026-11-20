import { useController } from '../../runtime'
import { Empty, Notice, PageHeader } from '../../runtime/ui'
import { Text, View } from '@tarojs/components'
import controller from './list.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, loading, error, needsLogin } = data
  return <View className='container'>
    <PageHeader title='公告' subtitle='当前生效的站点与平台通知' />
    {(items || []).map((item: any, index: number) => <View className='card' key={String(item?.id ?? index)}>
      <Text className='entity-title'>{item?.title}</Text>
      <Text className='text-secondary'>{item?.timeText}</Text>
      <Text className='prose'>{item?.content}</Text>
    </View>)}
    {loading && <Notice>正在读取公告…</Notice>}
    {error && <Notice tone='error' retry={event('load', {})} busy={loading}>{error}</Notice>}
    {!loading && !error && !items?.length && !needsLogin && <Empty icon='notification'>暂无生效公告</Empty>}
  </View>
}
