import { Button, Text, View } from '@tarojs/components'
import { useController } from '../../runtime'
import { Empty, Notice, PageHeader, Segments } from '../../runtime/ui'
import controller from './txns.controller'

export default function PageView() {
  const { data, event } = useController(controller)
  const { items, total, loading, error, needsLogin, statusIndex, statusNames } = data
  return <View className='container'>
    <PageHeader title='余额流水' subtitle='收入与支出分开查看' />
    {needsLogin && <View className='hero'><View className='hero-body'>
      <Text className='hero-title'>登录查看流水</Text>
      <Button type='primary' onClick={event('login', {})}>登录查看流水</Button>
    </View></View>}
    <Segments labels={statusNames} selected={statusIndex} onSelect={index => event('onStatus')({ detail: { value: index } })} />
    {(items || []).map((item: any, index: number) => <View className='card entity' key={String(item?.txn_no ?? index)}>
      <View className='row'><Text className='entity-title'>{item?.typeText}</Text><Text className={String(item?.amountText || '').startsWith('-') ? 'money-out' : String(item?.amountText || '').startsWith('+') ? 'success' : ''}>{item?.amountText} 元</Text></View>
      <Text>变动后余额 ¥{item?.balanceText}</Text>
      {item?.remark && <Text>{item.remark}</Text>}
      <Text className='text-secondary'>{item?.timeText}</Text>
      <Text className='text-secondary'>流水号 {item?.txn_no}</Text>
    </View>)}
    {loading && <Notice>正在读取流水…</Notice>}
    {error && <Notice tone='error' retry={event('retry', {})}>{error}</Notice>}
    {!loading && !error && !needsLogin && total === 0 && <Empty icon='list'>暂无资金流水</Empty>}
    {items?.length < total && !loading && <Text className='text-secondary load-more'>继续上滑加载更多</Text>}
  </View>
}
